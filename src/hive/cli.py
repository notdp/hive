"""CLI entry point for hive."""

from __future__ import annotations

import json
import os
import secrets
import shutil
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

import click

from . import bus
from . import context as hive_context
from . import notify_hook
from . import notify_ui
from . import plugin_manager
from . import skill_sync
from . import tmux
from .agent import AGENT_STARTUP_TIMEOUT, Agent
from .agent_cli import AGENT_CLI_NAMES, detect_profile_for_pane, member_role_for_pane, normalize_command, resolve_session_id_for_pane
from .team import HIVE_HOME, LEAD_AGENT_NAME, Team, Terminal


_COMMAND_HELP_SECTIONS = {
    # Daily — the default agent collaboration path.
    "current": "Daily",
    "init": "Daily",
    "team": "Daily",
    "send": "Daily",
    "reply": "Daily",
    "answer": "Daily",
    "suggest": "Daily",
    "notify": "Daily",
    # Handoff — spawn/fork a pane, load a workflow, or bring a pane into the team.
    "handoff": "Handoff",
    "fork": "Handoff",
    "spawn": "Handoff",
    "workflow": "Handoff",
    "register": "Handoff",
    # Debug — diagnostics, durable-store inspection, low-level pane control, rare admin.
    "doctor": "Debug",
    "delivery": "Debug",
    "thread": "Debug",
    "teams": "Debug",
    "activity": "Debug",
    "peer": "Debug",
    "capture": "Debug",
    "inject": "Debug",
    "interrupt": "Debug",
    "kill": "Debug",
    "exec": "Debug",
    "terminal": "Debug",
    "create": "Debug",
    "delete": "Debug",
    "layout": "Debug",
    # Plugin Helpers (human-only, unchanged).
    "cvim": "Plugin Helpers",
    "vim": "Plugin Helpers",
    "vfork": "Plugin Helpers",
    "hfork": "Plugin Helpers",
    # Extensions (unchanged).
    "plugin": "Extensions",
}
_COMMAND_HELP_SECTION_ORDER = [
    "Daily",
    "Handoff",
    "Debug",
    "Plugin Helpers",
    "Extensions",
    "Other Commands",
]
_COMMAND_HELP_SECTION_DESCRIPTIONS = {
    "Daily": "Daily agent path — inspect context, talk to peers, and pull the human in when necessary.",
    "Handoff": "Spawn or fork a pane, or load a workflow so another agent can pick up the work.",
    "Debug": "Troubleshoot delivery, runtime state, and low-level pane behavior. Not on the happy path.",
    "Plugin Helpers": "Human-only editor and split helpers backed by enabled plugin scripts. Droid exposes them as native slash commands (`/cvim`, `/vim`, ...); in Claude Code and Codex the human types them inline via the shell escape (e.g. `!hive cvim`). These are NOT meant for the model to call on its own.",
    "Extensions": "Manage first-party Hive plugins that materialize commands and skills for Factory, Claude Code, and Codex.",
}
_ROOT_HELP_EXAMPLES = '''# Inspect current tmux/Hive binding
hive current

# Show team members, peers, and runtime input/activity state
hive team

# Send a short message to a peer
hive send dodo "review this diff"

# Hand a thread off to another teammate
hive handoff dodo --artifact /tmp/task.md

# Answer a pending AskUserQuestion from another agent
hive answer dodo "yes"

# Find a good collaborator
hive suggest

# Send detailed context via stdin artifact (preferred for long content)
cat <<'EOF' | hive send dodo "see report" --artifact -
# Findings
- item
EOF'''

_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."
_TMUX_OPTIONAL_ROOT_COMMANDS = {"plugin", "_notify-hook"}
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
    payload = {
        "team": team_name,
        "workspace": workspace or "",
        "agent": agent_name,
        "role": role,
        "pane": current_pane,
        "tmuxSession": session_name,
        "tmuxWindow": window_target,
    }
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


def _load_team(team: str) -> Team:
    try:
        return Team.load(team)
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
        return discovered_team, _load_team(discovered_team)
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


def _resolve_repo_root(cwd: str | None = None) -> str:
    target_cwd = cwd or os.getcwd()
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=target_cwd,
            capture_output=True,
            text=True,
            check=False,
            timeout=2,
        )
    except Exception:
        return ""
    if result.returncode != 0:
        return ""
    return result.stdout.strip()


def _add_runtime_location_fields(
    payload: dict[str, object],
    *,
    workspace_key: str = "workspace",
) -> dict[str, object]:
    if "runtimeWorkspace" not in payload and workspace_key in payload:
        payload["runtimeWorkspace"] = payload.pop(workspace_key)
    cwd = os.getcwd()
    payload["cwd"] = cwd
    repo_root = _resolve_repo_root(cwd)
    if repo_root:
        payload["repoRoot"] = repo_root
    return payload


def _default_auto_workspace_path(session_name: str, window_id: str) -> Path:
    slug = window_id.lstrip("@") if window_id else "0"
    return Path(f"/tmp/hive-{session_name}-{slug}")


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


def _augment_current_payload_with_runtime(payload: dict[str, object], t: Team) -> dict[str, object]:
    if not payload.get("agent"):
        return payload
    from .sidecar import request_team_runtime

    ws = _resolve_workspace(t, required=False)
    if not ws:
        return payload
    _ensure_team_sidecar(t, ws)
    runtime = request_team_runtime(str(ws), team=t.name)
    if not runtime or runtime.get("ok") is False:
        return payload
    members = runtime.get("members")
    if not isinstance(members, dict):
        return payload
    member_runtime = members.get(str(payload["agent"]))
    if not isinstance(member_runtime, dict):
        return payload
    model = member_runtime.get("model")
    if model:
        payload["model"] = model
    return payload


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
            "model",
            "sessionId",
            "inputState",
            "inputReason",
            "pendingQuestion",
            "activityState",
            "activityReason",
            "activityObservedAt",
        ):
            value = runtime_fields.get(key)
            if value in ("", None):
                continue
            member[key] = value
    needs_answer = runtime.get("needsAnswer")
    if isinstance(needs_answer, list) and needs_answer:
        payload["needsAnswer"] = needs_answer
    return payload


def _team_status_payload(t: Team) -> dict[str, object]:
    payload = _augment_team_payload_with_runtime(t, t.status())
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
        content = sys.stdin.read()
        ws_artifacts = Path(workspace) / "artifacts"
        ws_artifacts.mkdir(parents=True, exist_ok=True)
        filename = f"{time.time_ns()}-{secrets.token_hex(2)}.txt"
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


def _warn_if_current_pane_hive_skill_is_stale() -> None:
    if not _stderr_is_interactive():
        return
    cli_name = _current_pane_agent_cli()
    if cli_name:
        skill_sync.maybe_warn_hive_skill_drift(cli_name)


@click.group(cls=SectionedHelpGroup)
@click.pass_context
def cli(ctx: click.Context):
    """Hive - tmux-first multi-agent collaboration runtime."""
    if ctx.resilient_parsing:
        return
    if any(arg in {"-h", "--help"} for arg in sys.argv[1:]):
        return
    skill_sync.check_version_upgrade()
    _warn_if_current_pane_hive_skill_is_stale()
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


def _exec_plugin_helper(plugin_name: str, command_name: str, args: tuple[str, ...]) -> None:
    """Forward execution to the materialized plugin command script.

    Replaces the current Python process with `bash <script> <args>` so the
    plugin helper (and any tmux popups it spawns) owns the terminal lifetime.
    """
    script = plugin_manager.find_installed_command(plugin_name, command_name)
    if script is None:
        _fail(
            f"plugin '{plugin_name}' is not enabled. Run "
            f"`hive plugin enable {plugin_name}` first."
        )
    os.execvp("bash", ["bash", str(script), *args])


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
    """Fork the current agent session into a new split pane."""
    if prompt and not join_as:
        _fail("--prompt requires --join-as")

    if join_as:
        _, target_team = _resolve_scoped_team(None, required=True)
        assert target_team is not None
        registered_agent, new_pane = _fork_registered_agent(
            t=target_team,
            pane_id=pane_id,
            split=split,
            join_as=join_as,
            wait_for_ready=bool(prompt),
            prompt=prompt,
        )
        del registered_agent
        click.echo(json.dumps({
            "pane": new_pane,
            "registered": join_as,
            "team": target_team.name,
        }, indent=2, ensure_ascii=False))
        return

    current_pane, profile, session_id, horizontal, source_cwd = _fork_source_details(pane_id, split)
    new_pane = tmux.split_window(current_pane, horizontal=horizontal, cwd=source_cwd or None, detach=False)
    tmux.send_keys(new_pane, profile.resume_cmd.format(session_id=session_id))


@cli.command("teams")
def teams_cmd():
    """List known teams."""
    _gc_dead_teams()
    from .team import list_teams
    rows = []
    for entry in list_teams():
        try:
            team = Team.load(entry["name"])
        except FileNotFoundError:
            continue
        rows.append({
            "name": team.name,
            "workspace": team.workspace,
            "tmuxSession": team.tmux_session,
            "tmuxWindow": team.tmux_window,
            "members": sorted({team.lead_name, *team.agents.keys()}),
        })
    click.echo(json.dumps(rows, indent=2, ensure_ascii=False))


@cli.command("current")
def current_cmd():
    """Show current Hive context."""
    _gc_dead_teams()
    discovered = _discover_tmux_binding()
    if discovered.get("team"):
        _, t = _resolve_scoped_team(str(discovered.get("team")), required=False)
        if t is not None:
            discovered = _augment_current_payload_with_runtime(discovered, t)
        pane_id = tmux.get_current_pane_id() or ""
        if pane_id:
            hive_context.save_context_for_pane(
                pane_id,
                team=discovered.get("team", ""),
                workspace=discovered.get("workspace", ""),
                agent=discovered.get("agent", ""),
            )
        click.echo(json.dumps(_add_runtime_location_fields(dict(discovered)), indent=2, ensure_ascii=False))
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
    result["hint"] = "No team bound. Run `hive init` to create one from this tmux window."

    click.echo(json.dumps(_add_runtime_location_fields(result), indent=2, ensure_ascii=False))


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


def _derive_terminal_name(seen: set[str]) -> str:
    suffix = 1
    candidate = f"term-{suffix}"
    while candidate in seen:
        suffix += 1
        candidate = f"term-{suffix}"
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
        "Use `hive team` to inspect the team and `hive send <name> <message>` to reply."
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
) -> Agent:
    agent = Agent(
        name=agent_name,
        team_name=team_name,
        pane_id=pane_id,
        cwd=cwd,
        cli=pane_cli,
    )
    t.agents[agent_name] = agent
    tmux.tag_pane(pane_id, "agent", agent_name, team_name, cli=pane_cli)
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


def _fork_source_details(pane_id: str, split: str) -> tuple[str, object, str, bool, str]:
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

    session_id = resolve_session_id_for_pane(current_pane, profile=profile)
    if not session_id:
        _fail(f"cannot determine session id for pane '{current_pane}'")

    source_cwd = tmux.display_value(current_pane, "#{pane_current_path}") or ""
    return current_pane, profile, session_id, horizontal, source_cwd


def _fork_registered_agent(
    *,
    t: Team,
    pane_id: str,
    split: str,
    join_as: str,
    wait_for_ready: bool = False,
    prompt: str = "",
) -> tuple[Agent, str]:
    _ensure_pane_in_scope(t, pane_id)
    window_target = t.tmux_window or tmux.get_current_window_target() or ""
    panes = tmux.list_panes_full(window_target) if window_target else []
    seen_names = _window_seen_names(t, panes)
    _claim_member_name(join_as, seen_names)

    current_pane, profile, session_id, horizontal, source_cwd = _fork_source_details(pane_id, split)

    new_pane = tmux.split_window(current_pane, horizontal=horizontal, cwd=source_cwd or None, detach=False)
    tmux.send_keys(new_pane, profile.resume_cmd.format(session_id=session_id))
    registered_agent = _register_agent_member(
        t,
        pane_id=new_pane,
        team_name=t.name,
        agent_name=join_as,
        pane_cli=profile.name,
        cwd=source_cwd or os.getcwd(),
        notify=False,
    )
    if wait_for_ready or prompt:
        if not tmux.wait_for_text(new_pane, profile.ready_text, timeout=AGENT_STARTUP_TIMEOUT):
            _fail(f"forked pane '{new_pane}' did not become ready before sending prompt")
        time.sleep(1)
    if prompt:
        registered_agent.send(prompt)
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
        f"Reply path: hive send {original_sender} \"<takeover>\" --reply-to {anchor_msg_id}",
        "Continue updates on that same --reply-to.",
        "Do not use hive reply for this handoff.",
    ]
    if note.strip():
        lines.append(f"Note: {note.strip()}")
    return "\n".join(lines)


def _handoff_announce_body(*, target_agent: str) -> str:
    return (
        f"Delegating this thread to {target_agent}. "
        "Their handoff message is in flight."
    )


def _register_existing_pane(
    t: Team,
    pane: tmux.PaneInfo,
    *,
    team_name: str,
    seen_names: set[str],
) -> tuple[str, str, Agent | Terminal]:
    role, pane_cli = _classify_pane(pane)
    tmux.clear_pane_tags(pane.pane_id)
    pane_cwd = tmux.display_value(pane.pane_id, "#{pane_current_path}") or os.getcwd()
    if role == "agent":
        agent_name = _derive_agent_name(seen_names)
        agent = _register_agent_member(
            t,
            pane_id=pane.pane_id,
            team_name=team_name,
            agent_name=agent_name,
            pane_cli=pane_cli,
            cwd=pane_cwd,
            notify=False,
        )
        return role, agent_name, agent

    terminal_name = _derive_terminal_name(seen_names)
    terminal = Terminal(name=terminal_name, pane_id=pane.pane_id)
    t.terminals[terminal_name] = terminal
    tmux.tag_pane(pane.pane_id, "terminal", terminal_name, team_name)
    return role, terminal_name, terminal


@cli.command("init")
@click.option("--name", "-n", default="", help="Team name (default: tmux session name)")
@click.option("--workspace", "-w", default="", help="Workspace path (default: /tmp/hive-<session>-<window>/)")
@click.option("--notify/--no-notify", default=True, help="Push hive skill + context to other panes")
def init_cmd(name: str, workspace: str, notify: bool):
    """Initialize a team from the current tmux window."""
    if not tmux.is_inside_tmux():
        _fail("hive init requires a tmux session. Start tmux first.")

    _gc_dead_teams()

    session_name = tmux.get_current_session_name() or "hive"
    window_index = tmux.get_current_window_index() or "0"
    window_id = tmux.get_current_window_id() or ""
    window_target = tmux.get_current_window_target()
    current_pane = tmux.get_current_pane_id()
    existing = _discover_tmux_binding()
    if existing.get("team"):
        click.echo(json.dumps(existing, indent=2, ensure_ascii=False))
        return
    bound_team = tmux.get_window_option(window_target, "hive-team") if window_target else ""
    if bound_team:
        try:
            loaded = Team.load(bound_team, prefer_pane=current_pane or "")
        except FileNotFoundError:
            loaded = None
        if loaded and loaded.tmux_window == window_target and loaded.status().get("members"):
            panes = tmux.list_panes_full(window_target) if window_target else []
            current_info = next((pane for pane in panes if pane.pane_id == current_pane), None)
            if current_info and (not current_info.team or current_info.team == bound_team):
                seen_names = _names_used_in_window(panes)
                seen_names.add(loaded.lead_name or LEAD_AGENT_NAME)
                role, member_name, member = _register_existing_pane(
                    loaded,
                    current_info,
                    team_name=bound_team,
                    seen_names=seen_names,
                )
                workspace_str = loaded.workspace or ""
                hive_context.save_context_for_pane(
                    current_pane or "",
                    team=bound_team,
                    workspace=workspace_str,
                    agent=member_name,
                )
                _remember_context(team=bound_team, workspace=workspace_str, agent=member_name)
                if notify and isinstance(member, Agent):
                    member.load_skill("hive")
                    member.send(_hive_join_message(member_name, bound_team))
                click.echo(json.dumps({
                    "team": bound_team,
                    "workspace": workspace_str,
                    "agent": member_name,
                    "role": role,
                    "pane": current_pane,
                    "tmuxSession": session_name,
                    "tmuxWindow": window_target,
                }, indent=2, ensure_ascii=False))
                return
            _fail(
                f"tmux window '{window_target}' already belongs to team '{bound_team}'; "
                "current pane is not registered"
            )
        for key in ("hive-team", "hive-workspace", "hive-desc", "hive-created", "hive-peers"):
            tmux.clear_window_option(window_target, f"@{key}")

    team_name = name or f"{session_name}-{window_index}"

    # Global duplicate check: another window may already own this team name
    # (e.g. stale tag left after a window move).
    from .team import _find_team_window, _gc_stale_team_windows
    existing_wt, _ = _find_team_window(team_name, prefer_pane=tmux.get_current_pane_id() or "")
    if existing_wt and existing_wt != window_target:
        # Stale tag on another window — clean it up so we can claim the name.
        _gc_stale_team_windows(team_name, keep=window_target or "", all_windows=[existing_wt])

    default_ws_path = _default_auto_workspace_path(session_name, window_id or window_index)
    using_auto_workspace = not workspace
    ws_path = Path(workspace).expanduser() if workspace else default_ws_path
    ws = str(ws_path)

    panes = tmux.list_panes_full(window_target) if window_target else []

    if using_auto_workspace:
        # A fresh `hive init` on the same tmux window should not inherit the
        # previous team's event log or artifacts from the default auto workspace.
        from .sidecar import stop_sidecar

        stop_sidecar(str(ws_path))
        bus.reset_workspace(ws_path)
    else:
        bus.init_workspace(ws_path)

    try:
        t = Team.create(team_name, description=f"auto-init from tmux {session_name}:{window_index}", workspace=str(ws_path))
    except ValueError as e:
        _fail(str(e))

    _remember_context(team=team_name, workspace=str(ws_path), agent=LEAD_AGENT_NAME)

    seen_names = _names_used_in_window(panes)
    seen_names.add(LEAD_AGENT_NAME)
    discovered = []
    for pane in panes:
        if pane.team and pane.team != team_name:
            _fail(f"pane '{pane.pane_id}' already belongs to team '{pane.team}'")
        role, _pane_cli = _classify_pane(pane)
        is_current = pane.pane_id == current_pane
        if is_current:
            discovered.append({
                "paneId": pane.pane_id,
                "role": role,
                "name": LEAD_AGENT_NAME,
                "command": pane.command,
                "isSelf": True,
            })
            continue

        role, member_name, member = _register_existing_pane(
            t,
            pane,
            team_name=team_name,
            seen_names=seen_names,
        )
        if isinstance(member, Agent):
            hive_context.save_context_for_pane(
                pane.pane_id, team=team_name, workspace=str(ws_path), agent=member_name,
            )
            if notify:
                member.load_skill("hive")
                member.send(_hive_join_message(member_name, team_name))
        discovered.append({
            "paneId": pane.pane_id,
            "role": role,
            "name": member_name,
            "command": pane.command,
            "isSelf": False,
        })

    # Start team sidecar for pending send tracking.
    _ensure_team_sidecar(t, ws_path)

    result = {
        "team": team_name,
        "workspace": str(ws_path),
        "window": window_target,
        "panes": discovered,
    }
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.command("register")
@click.argument("pane_id")
@click.option("--as", "name_override", default="", help="Name for the new member (default: auto-derived)")
@click.option("--notify/--no-notify", default=True, help="Push hive skill + join message to the pane")
def register_cmd(pane_id: str, name_override: str, notify: bool):
    """Register an external pane into the current team."""
    if not tmux.is_inside_tmux():
        _fail("hive register requires a tmux session.")

    binding = _discover_tmux_binding()
    team_name = binding.get("team")
    if not team_name:
        _fail("no team bound to the current window. Run `hive init` first.")

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
    if role == "agent":
        agent_name = name_override or _derive_agent_name(seen_names)
        _register_agent_member(
            t,
            pane_id=pane_id,
            team_name=team_name,
            agent_name=agent_name,
            pane_cli=pane_cli,
            cwd=tmux.display_value(pane_id, "#{pane_current_path}") or os.getcwd(),
            notify=notify,
        )
        member_name = agent_name
    else:
        terminal_name = name_override or _derive_terminal_name(seen_names)
        terminal = Terminal(name=terminal_name, pane_id=pane_id)
        t.terminals[terminal_name] = terminal
        tmux.tag_pane(pane_id, "terminal", terminal_name, team_name)
        member_name = terminal_name

    click.echo(json.dumps({
        "registered": member_name,
        "role": role,
        "pane": pane_id,
        "team": team_name,
    }, indent=2, ensure_ascii=False))


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
    """Spawn an agent pane."""
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
    """Delegate a thread via send/spawn/fork wrapper."""
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
                wait_for_ready=True,
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
            "state": "skipped",
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
                "state": "failed",
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


@cli.command("wait-status", hidden=True, context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("legacy_args", nargs=-1, type=click.UNPROCESSED)
def wait_status(legacy_args: tuple[str, ...]):
    """Removed legacy status polling command."""
    del legacy_args
    _status_migration_failure("wait-status")


@cli.command("inject")
@click.argument("agent_name")
@click.argument("text")
def inject_cmd(agent_name: str, text: str):
    """Debug: inject raw input into an agent pane."""
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    t.get(agent_name).send(text)
    click.echo(f"Injected raw input into {agent_name}.")


@cli.command("team")
def team_cmd():
    """Show team overview."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    click.echo(json.dumps(_team_status_payload(t), indent=2, ensure_ascii=False))


@cli.command(hidden=True)
def who():
    """Backward-compatible alias for `hive team`."""
    team_cmd.callback()  # type: ignore[attr-defined]


_LAYOUT_PRESETS = ("main-vertical", "main-horizontal", "tiled", "even-horizontal", "even-vertical")


@cli.command("layout")
@click.argument("preset", type=click.Choice(_LAYOUT_PRESETS, case_sensitive=False))
def layout_cmd(preset: str):
    """Apply a tmux layout preset to the current team window."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    window_target = t.tmux_window or tmux.get_current_window_target() or ""
    if not window_target:
        _fail("Cannot determine tmux window target")
    if preset in ("main-vertical", "main-horizontal"):
        dim = "main-pane-width" if preset == "main-vertical" else "main-pane-height"
        tmux.set_window_option(window_target, dim, "50%")
    tmux.select_layout(window_target, preset)
    click.echo(json.dumps({"layout": preset, "window": window_target}))


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
@click.option("--reply-to", default="", help="Message ID this is replying to")
@click.option(
    "--wait",
    is_flag=True,
    help="Block until transcript confirms delivery; if the runtime queue is visible first, state=queued and background tracking continues",
)
@click.option("--to", "to_option", hidden=True, default=None)
@click.option("--msg", "msg_option", hidden=True, default=None)
def send(
    to_agent: str,
    body: str,
    artifact: str,
    reply_to: str,
    wait: bool,
    to_option: str | None,
    msg_option: str | None,
):
    """Send a Hive message to another agent.

    `queued` / `pending` mean the message was accepted and background tracking continues.
    `confirmed` means delivery was confirmed in the initial send window.
    `failed` means local submit failed and should be retried.
    """
    _reject_legacy_recipient_options(to_option, msg_option, command="send", to_agent=to_agent)
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    sender = _resolve_sender(None)
    ws = _resolve_workspace(t, required=True)
    resolved_artifact = _resolve_artifact_path(artifact, workspace=ws)
    try:
        payload = _request_send_payload(
            workspace=ws,
            team=t,
            sender_agent=sender,
            target_agent=to_agent,
            body=body,
            artifact=resolved_artifact,
            reply_to=reply_to,
            wait=wait,
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
@click.option(
    "--wait",
    is_flag=True,
    help="Block until transcript confirms delivery; if the runtime queue is visible first, state=queued and background tracking continues",
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

    Without ``--reply-to``, hive picks the most recent send event from
    ``to_agent`` to you that you have not already replied to. If there
    is no such message, the command fails and asks you to pass
    ``--reply-to`` explicitly; ``hive reply`` never guesses across
    competing threads.
    """
    _reject_legacy_recipient_options(to_option, msg_option, command="reply", to_agent=to_agent)
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    sender = _resolve_sender(None)
    ws = _resolve_workspace(t, required=True)

    resolved_reply_to = reply_to_override
    if not resolved_reply_to:
        latest = bus.latest_inbound_send_event(ws, sender=sender, target=to_agent)
        if latest is None:
            _fail(
                f"no recent message from '{to_agent}' to '{sender}'; "
                "use 'hive send' or pass --reply-to explicitly"
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
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


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
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("message_id")
def delivery(message_id: str):
    """Check delivery status of a sent message by ID."""
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


@cli.command("suggest")
@click.argument("source_agent", required=False, default="")
def suggest(source_agent: str):
    """Suggest likely collaboration candidates."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    ws = _resolve_workspace(t, required=True)
    resolved_source = source_agent or _resolve_sender(None)
    from .sidecar import request_suggest

    _ensure_team_sidecar(t, ws)
    payload = request_suggest(str(ws), team=t.name, source_agent=resolved_source)
    if not payload:
        _fail("sidecar unavailable")
    if payload.get("ok") is False:
        _fail(str(payload.get("error", "suggest failed")))
    payload.pop("ok", None)
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("message_id")
def thread(message_id: str):
    """Show a reply thread rooted at a msgId."""
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
    """Diagnose agent connectivity and session state."""
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
    if include_skills:
        payload["skills"] = skill_sync.diagnose_hive_skill(_resolve_member_cli_name(t, target_name))
    click.echo(json.dumps(payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("agent_name", required=False, default="")
def activity(agent_name: str):
    """Classify transcript activity as active/idle/unknown."""
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
        _fail(str(payload.get("error", "activity probe failed")))

    activity_payload: dict[str, object] = {
        "agent": payload.get("agent", target_name),
        "team": payload.get("team", t.name),
        "activityState": payload.get("activityState", "unknown"),
        "activityReason": payload.get("activityReason", "unknown"),
    }
    for key in (
        "alive",
        "inputState",
        "cli",
        "model",
        "sessionId",
        "pane",
        "transcript",
        "transcriptExists",
        "transcriptSize",
        "activityObservedAt",
        "activityRole",
        "activityPartKinds",
        "activityEvidence",
    ):
        value = payload.get(key)
        if value in ("", None):
            continue
        activity_payload[key] = value
    click.echo(json.dumps(activity_payload, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("member_name")
@click.option("--lines", "-n", default=30)
def capture(member_name: str, lines: int):
    """Debug: capture raw pane output from any member (agent or terminal)."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    try:
        agent = t.get(member_name)
        click.echo(agent.capture(lines))
    except KeyError:
        if member_name in t.terminals:
            pane_id = t.terminals[member_name].pane_id
            click.echo(tmux.capture_pane(pane_id, lines))
        else:
            _fail(f"member '{member_name}' not found in team '{t.name}'")


@cli.command()
@click.argument("agent_name")
def interrupt(agent_name: str):
    """Interrupt an agent pane."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    t.get(agent_name).interrupt()
    click.echo(f"Interrupted {agent_name}.")


@cli.command()
@click.argument("agent_name")
def kill(agent_name: str):
    """Kill an agent pane and remove it from the team."""
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    agent = t.get(agent_name)
    agent.kill()
    if agent_name in t.agents:
        del t.agents[agent_name]
    click.echo(f"Killed {agent_name}.")


@cli.command("cvim", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def cvim_cmd(args: tuple[str, ...]) -> None:
    """Human-only: open vim seeded with the previous assistant message and send the diff back.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive cvim`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_plugin_helper("cvim", "cvim", args)


@cli.command("vim", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def vim_cmd(args: tuple[str, ...]) -> None:
    """Human-only: open a blank vim buffer and send the final result back to the agent pane.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive vim`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_plugin_helper("cvim", "vim", args)


@cli.command("vfork", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def vfork_cmd(args: tuple[str, ...]) -> None:
    """Human-only: fork the current Hive session into a vertical split.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive vfork`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_plugin_helper("fork", "vfork", args)


@cli.command("hfork", context_settings={"ignore_unknown_options": True, "allow_extra_args": True})
@click.argument("args", nargs=-1, type=click.UNPROCESSED)
def hfork_cmd(args: tuple[str, ...]) -> None:
    """Human-only: fork the current Hive session into a horizontal split.

    Intended to be typed by the human via the agent's shell escape (e.g. `!hive hfork`)
    in Claude Code or Codex. Not meant for the model to invoke on its own.
    """
    _exec_plugin_helper("fork", "hfork", args)


@cli.command("notify")
@click.argument("message")
@click.option("--seconds", default=12, type=int, show_default=True, help="Overlay/highlight duration")
@click.option("--highlight/--no-highlight", default=False, help="Flash target pane border")
@click.option("--window-status/--no-window-status", default=True, help="Flash tmux window status")
def notify_cmd(
    message: str,
    seconds: int,
    highlight: bool,
    window_status: bool,
):
    """Notify the user for the current pane."""
    target_pane = _resolve_target_pane()
    payload = notify_ui.notify(
        message,
        target_pane,
        seconds=max(1, seconds),
        highlight=highlight,
        window_status=window_status,
    )
    click.echo(json.dumps(payload))


@cli.command("_notify-hook", hidden=True)
def notify_hook_cmd() -> None:
    """Handle Droid hook payloads for notify plugin internals."""
    raise SystemExit(notify_hook.main())


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


# --- Terminal commands ---

def _resolve_terminal(t: Team, name: str) -> Terminal:
    if name not in t.terminals:
        _fail(f"terminal '{name}' not found in team '{t.name}'")
    terminal = t.terminals[name]
    _ensure_pane_in_scope(t, terminal.pane_id)
    if not terminal.is_alive():
        _fail(f"terminal '{name}' pane is no longer alive")
    return terminal


@cli.command("exec")
@click.argument("terminal_name")
@click.argument("command")
def exec_cmd(terminal_name: str, command: str):
    """Debug: inject a command into a terminal pane."""
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    terminal = _resolve_terminal(t, terminal_name)
    tmux.send_keys(terminal.pane_id, command)
    click.echo(f"Sent to {terminal_name} ({terminal.pane_id}).")


@cli.group()
def terminal():
    """Manage terminal panes in the team."""
    pass


@terminal.command("add")
@click.argument("name", required=False, default="")
@click.option("--pane", "pane_id", default="", help="Pane ID (default: current pane)")
def terminal_add(name: str, pane_id: str):
    """Register a pane as a terminal."""
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    resolved_pane = pane_id or tmux.get_current_pane_id()
    if not resolved_pane:
        _fail("cannot determine pane ID (are you in tmux?)")
    _ensure_pane_in_scope(t, resolved_pane)
    term_name = name or f"term-{len(t.terminals) + 1}"
    if term_name in t.terminals:
        _fail(f"terminal '{term_name}' already exists")
    t.terminals[term_name] = Terminal(name=term_name, pane_id=resolved_pane)
    tmux.tag_pane(resolved_pane, "terminal", term_name, team_name)
    click.echo(f"Terminal '{term_name}' registered ({resolved_pane}).")


@terminal.command("remove")
@click.argument("name")
def terminal_remove(name: str):
    """Unregister a terminal pane."""
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    if name not in t.terminals:
        _fail(f"terminal '{name}' not found")
    terminal_obj = t.terminals.pop(name)
    if tmux.is_pane_alive(terminal_obj.pane_id):
        tmux.clear_pane_tags(terminal_obj.pane_id)
    click.echo(f"Terminal '{name}' removed.")


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
