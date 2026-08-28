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
import uuid
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

import click

from . import bus
from . import context as hive_context
from . import notify_ui
from . import plugin_manager
from . import tmux
from .agent import AGENT_STARTUP_TIMEOUT, Agent, _submit_interactive_text
from .agent_cli import AGENT_CLI_NAMES, detect_profile_for_pane, member_role_for_pane, normalize_command, resolve_session_id_for_pane
from .team import HIVE_HOME, LEAD_AGENT_NAME, Team


_COMMAND_HELP_SECTIONS = {
    # Daily — per-turn agent collaboration loop.
    "init": "Daily",
    "team": "Daily",
    "send": "Daily",
    "ccd": "Daily",
    "notify": "Daily",
    "compact": "Daily",
    "skills": "Daily",
    # Panes — bring up another agent pane (fresh or forked).
    "fork": "Panes",
    "spawn": "Panes",
    # Workflow — higher-level flows on top of Hive.
    "flow": "Workflow",
    "worktree": "Workflow",
    "pr": "Workflow",
    "ls": "Workflow",
    "resume": "Workflow",
    # Team — wire up the tmux team around the current window.
    "create": "Team",
    "delete": "Team",
    "register": "Team",
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
    # Launchers — hive-managed claude/codex/grok entry points + shell integration.
    "claude": "Launchers",
    "codex": "Launchers",
    "grok": "Launchers",
    "shell-init": "Launchers",
}
_COMMAND_HELP_SECTION_ORDER = [
    "Daily",
    "Panes",
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
    "Panes": "Bring up another agent pane — a fresh spawn or a forked clone.",
    "Workflow": "Higher-level flows on top of Hive: worktrees, PR anchors, team snapshots.",
    "Team": "Create, extend, and wire up the tmux team around the current window.",
    "Human Helpers": "Popup editor and split helpers for the human (not the model). In Claude Code / Codex, type `!hive cvim` via shell escape. Requires tmux >= 3.2.",
    "Debug": "Troubleshoot delivery, runtime state, and low-level pane behavior. Not on the happy path.",
    "Extensions": "Manage first-party Hive plugins (Claude Code, Codex).",
    "Launchers": (
        "hive-managed launchers behind the `hcodex` / `hclaude` / `hgrok` shell functions "
        "from `hive shell-init`, rarely run by hand. All arguments are forwarded verbatim, "
        "so `hive claude --help` shows claude's own help, not this wrapper's."
    ),
}
_ROOT_HELP_EXAMPLES = '''# Team lifecycle
hive init                                    # make this pane the orch of a new team
hive spawn explore --task /tmp/task.md       # spawn a member and dispatch its task atomically
hive team                                    # members + runtime state (busy / inputState / turnPhase)

# Messaging (root thread: body is a short summary, details go in --artifact)
hive send dodo "review this diff" --artifact /tmp/diff.md
hive send dodo "see report" --artifact - <<'EOF'
# Findings
- item
EOF

# Fork, spawn
hive fork                                    # split the current pane into a clone
hive spawn claude                            # bring up a new agent pane

# Debug connectivity
hive doctor dodo                             # probe a peer's connectivity'''

_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."
_TMUX_OPTIONAL_ROOT_COMMANDS = {"plugin", "config", "shell-init", "codex", "claude", "grok", "resume-hint", "skills", "worktree", "ls", "ccd"}


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

def _resolve_sender(agent_name: str | None) -> str:
    return agent_name or _default_agent() or LEAD_AGENT_NAME


def _load_team(team: str, *, prefer_pane: str = "") -> Team:
    try:
        return Team.load(team, prefer_pane=prefer_pane)
    except FileNotFoundError:
        click.echo(f"Error: team '{team}' not found", err=True)
        sys.exit(1)

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
    """Window-id-derived team name — the overflow scheme behind the pool.

    tmux window ids are never reused within a session, which avoids the
    cross-window collisions that window-index-derived names hit after
    break-out or window reorder (Bug A).
    """
    return f"{session_name}-{_window_id_slug(window_id, window_index)}"


TEAM_NAME_POOL: tuple[str, ...] = (
    "honey",
    "comb",
    "wasp",
    "bumble",
    "hornet",
    "nectar",
    "pollen",
    "amber",
    "clover",
    "sage",
)


def _claimed_group_namespaces() -> set[str]:
    """Group tags and qualified ``@hive-agent`` prefixes claimed by live panes.

    A pane with ``@hive-agent=krays.coco`` claims ``krays`` even without a
    group tag — the qualified resolver can route to it, so a new team must
    not take that name.
    """
    claimed: set[str] = set()
    for pane in tmux.list_panes_all():
        group = (pane.group or "").strip()
        if group:
            claimed.add(group)
        agent = (pane.agent or "").strip()
        if "." in agent:
            prefix, _, _ = agent.partition(".")
            if prefix:
                claimed.add(prefix)
    return claimed


def _pick_team_name(session_name: str, window_id: str, window_index: str = "0") -> str:
    """Short memorable name for a new team; window-id scheme as overflow.

    The name is a routing key (`hive send <team>.<member>`), so it must be
    short and typeable. Claimed = any live pane's team tag, plus any group
    tag or qualified `@hive-agent` prefix (both route in qualified-name
    lookup). `_claim_team_name` stays the final anti-clobber; identity
    itself binds to the window via tags, never to the name's shape.
    """
    used = {p.team for p in tmux.list_panes_all() if p.team}
    used |= _claimed_group_namespaces()
    for candidate in TEAM_NAME_POOL:
        if candidate not in used:
            return candidate
    return _default_team_name_for_window(session_name, window_id, window_index)


def _team_default_auto_workspace_path(team: Team) -> Path | None:
    if not team.tmux_session:
        return None
    window_id = getattr(team, "tmux_window_id", "") or ""
    if not window_id and team.tmux_window and ":" in team.tmux_window:
        window_id = team.tmux_window.rsplit(":", 1)[-1]
    if not window_id:
        return None
    return _default_auto_workspace_path(team.tmux_session, window_id)

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


_CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS = {
    "claude",
    "codex",
    "config",
    "doctor",
    "grok",
    "inject",
    "plugin",
    "resume-hint",
    "shell-init",
    "skills",
}


def _is_codex_tool_env() -> bool:
    return bool(os.environ.get("CODEX_THREAD_ID", "").strip())


def _codex_pane_from_thread_env() -> str:
    """Pane recorded for this codex tool's own thread, or ''.

    Codex injects the thread's ``CODEX_THREAD_ID`` into every tool
    subprocess; hive records which pane each thread is bound to. A resolvable
    mapping is what makes a codex hive-native — env TMUX_PANE is the shared
    daemon's frozen value and never identity.
    """
    thread_id = os.environ.get("CODEX_THREAD_ID", "").strip()
    if not thread_id:
        return ""
    from .adapters import codex_app_server

    return codex_app_server.pane_for_thread(thread_id) or ""


def _codex_relaunch_message() -> str:
    return (
        "this codex isn't hive-managed — hive runtime is degraded.\n"
        "for future launches use hcodex (one-time setup, any shell):\n"
        "  grep -q 'hive shell-init' ~/.zshrc || "
        "echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n"
        "then exit this codex (Ctrl-C twice) and run: hive codex resume"
    )


def _require_codex_native(invoked: str | None) -> None:
    if invoked in _CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS:
        return
    if not _is_codex_tool_env() or _codex_pane_from_thread_env():
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
        if ctx.invoked_subcommand == "send":
            from .adapters import claude_sessions

            if claude_sessions.self_session() is not None:
                return  # a Claude session sending into hive as a guest
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
    Agents also invoke it to create a clone that can pick up work without
    interrupting the current turn.

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
    if name_override == "flow":
        _fail("'flow' is the flow runner's reserved mailbox address, not a member name")
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
        "Context is pre-bound. Run `/hive:hive` first and follow "
        "that protocol. Hive messages will arrive inline as "
        "<HIVE ...> ... </HIVE> blocks. "
        "Use `hive team` to inspect the team; message any peer with "
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
                "nothing was registered. Fix the inbox/daemon and retry, "
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
    env_entries: tuple[str, ...] = (),
    cli_name: str | None = None,
) -> Agent:
    resolved_cli_name = _resolve_spawn_cli_name(cli_name)
    from .agent_cli import validate_spawn_model

    model_error = validate_spawn_model(resolved_cli_name, model)
    if model_error:
        raise ValueError(model_error)
    extra_env = _parse_entries(env_entries) if env_entries else {}
    agent = t.spawn(
        agent_name,
        model=model,
        prompt=prompt,
        cwd=cwd,
        skill=skill,
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

    # Both clones launch through hive's managed launcher: the forked claude
    # binds its own cross-session inbox at startup, and `hive codex fork
    # <sid>` forks the thread server-side on the shared daemon, records the
    # fork as the new pane's thread, and resumes it — so a forked member joins
    # daemon-backed like a spawned one instead of the embedded codex this used
    # to refuse.
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
    """Resolve the team that owns *to_agent* for a send.

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


def _resolve_guest_send_target(to_agent: str, team: str) -> tuple[str, Team]:
    """Target resolution for a Claude-session guest (outside tmux).

    A guest has no scoped context and must never inherit one from a saved
    slot: resolve by scanning live pane tags, or by the explicit
    `<team>.<member>` address.
    """
    if team:
        t = _load_team(team)
        if _existing_team_agent(t, to_agent) is None:
            _fail(f"agent '{to_agent}' not found in team '{team}'")
        return t.name, t
    candidates = [p for p in tmux.list_panes_all() if p.agent == to_agent and p.team]
    teams = sorted({p.team for p in candidates})
    if not teams:
        _fail(f"agent '{to_agent}' not found in any live team (see `hive ls`)")
    if len(teams) > 1:
        addresses = ", ".join(f"{name}.{to_agent}" for name in teams)
        _fail(f"agent '{to_agent}' exists in {len(teams)} teams; address one of: {addresses}")
    return teams[0], _load_team(teams[0])


def _live_member_pids() -> dict[int, tuple[str, str]]:
    """pid -> (team, agent) for every live claude team-member engine.

    A claude member's process is its bg job's engine (found through the
    pane's job record), never anything on the pane tty — the tty only holds
    the attach viewer, whose pid must not shadow the member in the session
    registry.
    """
    from .adapters import claude_bg

    out: dict[int, tuple[str, str]] = {}
    for p in tmux.list_panes_all():
        if p.team and p.agent:
            job_id = claude_bg.job_id_for_pane(p.pane_id)
            engine = claude_bg.engine_session_for_job(job_id) if job_id else None
            if engine is not None:
                out[engine.pid] = (p.team, p.agent)
    return out


def _send_to_ccd_session(label: str, message: str, artifact: str) -> None:
    """`hive send ccd.<session>`: a member pushes into an outside Claude
    session's cross-session inbox.

    LABEL is the session's Claude Code name, its desktop title, or its pid,
    as listed by `hive ccd ls`. The receiving session reads the message
    between tool calls, or starts a turn with it when idle.

    Fire-and-forget: `accepted` means a live session took the frame. Whether
    its `crossSessionInbound` setting delivers it or holds it for a click is
    that session's decision and is not reported back here.
    """
    from .adapters import claude_sessions

    team, agent = _default_team(), _default_agent()
    if not (team and agent):
        _fail(
            "`ccd.<session>` is a team member's outbound address; another "
            "Claude session is messaged with the native SendMessage tool"
        )
    if artifact:
        _fail("a session push carries no --artifact; put the path in the body")
    if not message:
        _fail("message body required")
    matches = claude_sessions.resolve(label)
    if not matches:
        _fail(f"no live Claude session named, titled or numbered '{label}' (see `hive ccd ls`)")
    if len(matches) > 1:
        where = ", ".join(f"{s.name} (pid {s.pid}, {s.cwd or '?'})" for s in matches)
        _fail(f"{len(matches)} live sessions answer to '{label}': {where}; use the name or pid")
    target = matches[0]
    member = _live_member_pids().get(target.pid)
    if member is not None:
        m_team, m_agent = member
        if m_team == team:
            _fail(
                f"'{label}' is your teammate {m_agent}; members talk over "
                f"the bus: `hive send {m_agent}`"
            )
        _fail(f"'{label}' is {m_team}.{m_agent}, a member of another team, not an outside session")
    sender = f"{team}.{agent}"
    # The frame's `from` reaches only the human's message card; the receiving
    # model sees just the text. Wrap the body in the ordinary <HIVE> envelope
    # so the sender travels in band and the receiver answers by copying it
    # verbatim: `hive send <team>.<agent>`. No msgId: this is not a bus thread.
    from .runtime_state import format_hive_envelope

    envelope = format_hive_envelope(
        from_agent=sender,
        to_agent=f"ccd.{target.name}",
        body=message,
    )
    outcome = claude_sessions.send(
        target.socket_path, envelope, sender=sender, session_id=target.session_id
    )
    if outcome is None:
        _fail(
            f"session '{target.name}' (pid {target.pid}) is not listening on "
            f"{target.socket_path}; it may have just exited"
        )
    if outcome == claude_sessions.WRITE_TIMED_OUT:
        _fail(
            f"session '{target.name}' (pid {target.pid}) accepted the connection but did "
            f"not read the message (~{max(1, len(message) // 1024)} KB) in time; it looks "
            "stalled and may hold a truncated frame — retry once it is responsive"
        )
    # Fire-and-forget: success is silent (rule of silence); failures above
    # already exited non-zero with the reason.


def _existing_team_agent(t: Team, agent_name: str) -> Agent | None:
    try:
        return t.get(agent_name)
    except KeyError:
        return None


def _require_daemon_backed(pane: str) -> None:
    """Refuse to let an unmanaged codex join; point to the fix.

    A hive-manageable codex has a recorded thread on the shared app-server
    daemon (the managed launcher / spawn wrote it), which is what native
    runtime and delivery ride on. A bare `codex` — embedded, or remote but
    never recorded — gives hive no thread identity to bind, so rather than
    register a degraded member, stop here and tell the user how to relaunch
    it managed; ``hive codex resume`` preserves the session.
    """
    if _is_codex_tool_env():
        # Running from inside the codex TUI's own tool: the thread record is
        # the identity, and the shared daemon must answer.
        from .adapters import codex_app_server

        if _codex_pane_from_thread_env() and codex_app_server.daemon_alive():
            return
        _fail(_codex_relaunch_message())
    if not pane:
        return
    profile = detect_profile_for_pane(pane)
    if not profile:
        return
    if profile.name == "grok":
        _require_grok_leader_backed(pane)
        return
    if profile.name == "claude":
        _require_claude_job_backed(pane)
        return
    if profile.name != "codex":
        return
    from .adapters import codex_app_server

    if codex_app_server.thread_id_for_pane(pane) and codex_app_server.daemon_alive():
        return  # recorded thread on a live shared daemon — hive-managed, fine
    _fail(
        "this codex is not hive-managed; hive needs its thread on the shared "
        "app-server daemon for native runtime, so it can't join yet.\n"
        "for future launches use hcodex (one-time setup, any shell):\n"
        "  grep -q 'hive shell-init' ~/.zshrc || "
        "echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n"
        "for this session now (your session is preserved):\n"
        "  1) exit codex: press Ctrl-C (twice)\n"
        "  2) run: hive codex resume <session-id>   (or `hive codex resume` "
        "for the picker)\n"
        "then re-run /hive."
    )


def _require_claude_job_backed(pane: str) -> None:
    """Refuse a bare interactive claude pane: hive claude members run as bg
    jobs (engine on claude's supervisor, pane is an attach viewer), which is
    what delivery, runtime and resume all key on. A TUI claude would join
    with no job identity and every delivery to it would fail."""
    from .adapters import claude_bg

    if claude_bg.job_id_for_pane(pane):
        return
    _fail(
        "this claude is not hive-managed; hive claude members run as "
        "background jobs (`claude --bg`) with the pane attached as a viewer, "
        "so it can't join yet.\n"
        "for future launches use hclaude (one-time setup, any shell):\n"
        "  grep -q 'hive shell-init' ~/.zshrc || "
        "echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n"
        "for this session now (your session is preserved):\n"
        "  1) note your session id (`claude --resume` lists it), exit claude\n"
        "  2) run: hive claude -r <session-id>\n"
        "then re-run /hive."
    )


def _require_grok_leader_backed(pane: str) -> None:
    """Refuse a plain grok pane: hive delivers only through the pane leader."""
    from .adapters import grok_leader

    sock = grok_leader.pane_socket_path(pane)
    if sock.exists() and grok_leader.probe_socket(str(sock)):
        return
    sid = grok_leader.session_id_for_pane(pane) or ""
    resume = f"hive grok --resume {sid}" if sid else "hive grok"
    _fail(
        "this grok has no hive leader; hive delivers to grok only through the "
        "pane leader, so it can't join yet.\n"
        "for future launches use hgrok (one-time setup, any shell):\n"
        "  grep -q 'hive shell-init' ~/.zshrc || "
        "echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n"
        "for this session now (your session is preserved):\n"
        "  1) exit grok: /exit\n"
        f"  2) run: {resume}\n"
        "then re-run /skills hive."
    )


def _create_orch_team(*, current_pane: str) -> dict[str, object]:
    """Bind the current pane as the orch of a fresh team.

    Spawns nobody — members come later via `hive spawn`, driven by the orch.
    Placement: a lone pane binds its window in place; a crowded window
    breaks the orch pane out to a fresh one first, so team identity
    derives from the final window (Bug A).
    Idempotent: an already-bound pane returns its existing binding.
    """
    _gc_dead_teams()
    plugin_manager.cleanup_retired_plugins()

    existing = _discover_tmux_binding()
    if existing.get("team"):
        return dict(existing)

    session_name = tmux.get_current_session_name() or "hive"
    orch_cli = _resolve_spawn_cli_name(None)
    window = tmux.get_pane_window_target(current_pane) or ""
    if not window:
        _fail("cannot determine current window")
    panes = tmux.list_panes_full_or_none(window)
    if panes is None:
        _fail(f"tmux did not answer the pane listing for {window}; rerun init")
    if not any(p.pane_id == current_pane for p in panes):
        _fail(f"current pane {current_pane} missing from {window} listing; rerun init")

    orch_pane = current_pane
    if len(panes) >= 2:
        # Crowded window — isolate the orch so the team owns its window.
        new_window, orch_pane = tmux.break_pane(current_pane)
        if not new_window:
            _fail("failed to break out into a new window")
        window = new_window

    final_window_id = tmux.get_window_id(window) or ""
    final_index = window.rsplit(":", 1)[-1] if ":" in window else "0"

    team_name = _pick_team_name(session_name, final_window_id, final_index)
    _prepare_window_for_new_team(window, current_pane=orch_pane)
    _claim_team_name(team_name, this_window=window, explicit=False)
    from . import resume as resume_store

    # A recycled pool name must not inherit the dead predecessor's
    # snapshot (resume-hint would hand out a foreign sessionId).
    resume_store.archive_stale_snapshot(team_name)

    ws_path = _default_auto_workspace_path(session_name, final_window_id, final_index)
    # A fresh team on a reused window must not inherit the previous team's
    # event log or artifacts from the default auto workspace.
    from .sidecar import stop_sidecar

    stop_sidecar(str(ws_path))
    bus.reset_workspace(ws_path)

    try:
        t = Team.create_for_window(
            team_name,
            window_target=window,
            lead_pane_id=orch_pane,
            lead_name=LEAD_AGENT_NAME,
            description=f"auto-init from tmux {session_name} ({window})",
            workspace=str(ws_path),
            tag_lead=False,
        )
    except ValueError as e:
        _fail(str(e))
        raise AssertionError("unreachable")

    tmux.rename_window(window, t.name)
    tmux.configure_hive_window(window)
    tmux.set_pane_option(orch_pane, "hive-role", "agent")
    tmux.set_pane_option(orch_pane, "hive-agent", LEAD_AGENT_NAME)
    tmux.set_pane_option(orch_pane, "hive-team", t.name)
    tmux.set_pane_option(orch_pane, "hive-cli", orch_cli)
    hive_context.save_context_for_pane(
        orch_pane, team=t.name, workspace=str(ws_path), agent=LEAD_AGENT_NAME
    )
    _remember_context(team=t.name, workspace=str(ws_path), agent=LEAD_AGENT_NAME)
    _ensure_team_sidecar(t, ws_path)
    tmux.select_window(window)

    return {
        "team": t.name,
        "window": window,
        "orch": {"pane": orch_pane, "name": LEAD_AGENT_NAME, "cli": orch_cli},
        "workspace": str(ws_path),
        "protocol": "/hive:orch",
    }


@cli.command("init")
def init_cmd():
    """Make the current pane the orch of a fresh team.

    Binds the window, names the team, starts the sidecar — and spawns
    nobody. Members are created on demand with `hive spawn <name> --task`.
    Team name and workspace derive from the final window. Idempotent:
    re-running in a bound window reports the existing binding.
    """
    if not tmux.is_inside_tmux():
        _fail("hive init requires a tmux session. Run `tmux new-session` or `tmux attach` first, then rerun.")
    current_pane = tmux.get_current_pane_id() or ""
    if not current_pane:
        _fail("cannot determine current pane")
    if detect_profile_for_pane(current_pane) is None:
        _fail("current pane must be running claude / codex / grok (this becomes the orch)")
    _require_daemon_backed(current_pane)

    result = _create_orch_team(current_pane=current_pane)
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


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
@click.option("--model", "-m", default="", help="Model ID. claude: prefer aliases (fable/opus/sonnet) — they always track the latest; codex/grok: checked against the CLI's own catalog")
@click.option("--prompt", "-p", default="", help="Initial prompt (typed into TUI after startup)")
@click.option("--cwd", default="", help="Working directory")
@click.option("--skill", default="hive:hive", help="Base skill to load after startup ('none' to skip)")
@click.option("--env", "-e", multiple=True, help="Extra env vars (KEY=VALUE, repeatable)")
@click.option("--cli", "cli_name", type=click.Choice(["claude", "codex", "grok"]), default=None, help="Agent CLI to spawn (default: same as current pane)")
@click.option(
    "--task",
    "task_artifact",
    default=None,
    type=click.Path(exists=True, dir_okay=False),
    help="Task artifact to dispatch atomically once the member is ready (member never boots into an empty inbox)",
)
def spawn(agent_name: str, model: str, prompt: str,
          cwd: str, skill: str, env: tuple[str, ...], cli_name: str | None,
          task_artifact: str | None):
    """Spawn an agent pane, optionally dispatching a task atomically.

    Creates a new tmux pane in the current window and starts the chosen
    agent CLI. By default spawns the same CLI as the current pane; use
    `--cli claude|codex|grok` to pick a specific one.

    With `--task <artifact>`, the member boots straight into the member
    contract (`/hive:hive`) and the task artifact arrives as its first
    `<HIVE>` message — spawn and dispatch are one atomic step, so the
    member never wanders off exploring while waiting for work.

    \b
    Examples:
      hive spawn explore --task /tmp/tasks/explore.md
      hive spawn review --cli codex --task /tmp/tasks/review.md
      hive spawn dodo --cli codex
      hive spawn claude -m claude-opus-5 --skill none
    """
    if task_artifact and prompt:
        _fail("--task and --prompt are mutually exclusive (the task rides the message, not the birth prompt)")
    team_name, t = _resolve_scoped_team(None, required=True)
    assert team_name is not None and t is not None
    try:
        agent = _spawn_team_agent(
            t,
            team_name=team_name,
            agent_name=agent_name,
            model=model,
            prompt=("" if task_artifact else prompt),
            cwd=cwd,
            skill=("hive:hive" if task_artifact else skill),
            env_entries=env,
            cli_name=cli_name,
        )
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)
    if not task_artifact:
        click.echo(f"Agent '{agent_name}' spawned in pane {agent.pane_id}")
        return

    workspace = _resolve_workspace(t, required=True)
    _ensure_team_sidecar(t, Path(workspace))
    if agent.cli != "claude":
        # A claude member's inbox is a queue: the task can land while the
        # bootstrap turn is still running and waits its turn. Only CLIs
        # whose delivery injects into a live TUI need the ready gate.
        not_ready = _wait_for_peer_ready(
            workspace,
            team_name=team_name,
            agents={agent_name},
        )
        if not_ready:
            click.echo(json.dumps({
                "status": "spawn_ready_timeout",
                "agent": agent_name,
                "pane": agent.pane_id,
                "hint": "pane spawned but did not reach ready within 30s; dispatch manually via `hive send`",
            }, indent=2))
            sys.exit(1)

    task_path = str(Path(task_artifact).resolve())
    sender = _resolve_sender(None)
    try:
        _request_send_payload(
            workspace=workspace,
            team=t,
            sender_agent=sender,
            target_agent=agent_name,
            body=f"task dispatch: {Path(task_path).name}",
            artifact=task_path,
            command_name="spawn-dispatch",
            warn_on_long_body=False,
        )
    except RuntimeError as exc:
        click.echo(json.dumps({
            "status": "dispatch_failed",
            "agent": agent_name,
            "pane": agent.pane_id,
            "error": str(exc),
            "hint": f"member is ready but dispatch failed; retry: hive send {agent_name} ... --artifact {task_path}",
        }, indent=2))
        sys.exit(1)
    click.echo(json.dumps({
        "agent": agent_name,
        "pane": agent.pane_id,
        "task": task_path,
        "dispatched": True,
    }, indent=2))


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
    try:
        agent = t.get(agent_name)
    except KeyError:
        _fail(f"member '{agent_name}' not found in team '{t.name}'")
        return
    # Documented low-level bypass: raw composer keystrokes for every CLI, so
    # delivery paths (channel/RPC) can be debugged from outside themselves.
    # A claude member's keystrokes are piped into its bg job rather than its
    # pane, and the submit fails loudly when the engine did not take them.
    try:
        _submit_interactive_text(agent.pane_id, text, agent.cli)
    except RuntimeError as exc:
        _fail(str(exc))
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
    if target.cli in ("codex", "grok"):
        # Daemon-backed CLIs: an idle agent compacts via the dedicated RPC
        # (codex thread/compact/start, grok x.ai/compact_conversation) — never a
        # prompt, which only feeds the model the literal "/compact". When the
        # agent is busy (compact_pane returns non-"compacted") we do NOT queue or
        # silently defer: a Compact turn would abort the running turn, so instead
        # we keystroke `/compact` into the CLI's own TUI, which then shows its
        # native "disabled while a task is in progress." That is an explicit
        # refusal the agent can see, not a silent background compaction it never
        # learns about.
        from .adapters import codex_app_server, grok_leader
        transport = codex_app_server if target.cli == "codex" else grok_leader
        status = transport.compact_pane(target.pane_id)
        if status != "compacted":
            _submit_interactive_text(target.pane_id, "/compact", target.cli)
        return status
    # claude (and embedded codex without a daemon): `/compact` is a TUI
    # slash command, so it must go through the composer. For a claude member
    # that composer is reached by piping into `claude attach <jobId>`, and
    # the command is confirmed by its `<command-name>` transcript record.
    try:
        _submit_interactive_text(target.pane_id, "/compact", target.cli)
    except RuntimeError as exc:
        _fail(str(exc))
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
    result["hint"] = "No team bound. Run `hive init` to make this pane the orch of a fresh team, then spawn members with `hive spawn <name> --task <artifact>`."
    window_id = tmux.get_current_window_id() or ""
    if session_name and window_id:
        result["runtimeWorkspace"] = str(_default_auto_workspace_path(session_name, window_id))
    click.echo(json.dumps(_add_runtime_location_fields(result), indent=2, ensure_ascii=False))


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
                f"'{existing}' — run from a team pane, or start the team elsewhere."
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


@cli.group("flow")
def flow_cmd():
    """Deterministic member orchestration from a Python script.

    A flow script uses the `hive.flow` library: `agent()` spawns a live
    member pane, dispatches a task atomically, and blocks for the reply;
    `parallel()` fans out. Every node is a visible pane — watch, type
    into, or interrupt any of them while the flow runs.
    """


@flow_cmd.command("run")
@click.argument("script", type=click.Path(exists=True, dir_okay=False))
def flow_run_cmd(script: str):
    """Run SCRIPT against the current team.

    The script is trusted Python (you or your orch wrote it). Members it
    spawns reply to the reserved `flow` mailbox; the runner blocks until
    the script finishes. Typical use from an orch: run it in a background
    shell and read the output when it completes.

    \b
    Example script:
      from hive.flow import agent, parallel
      findings = agent("explore auth; write /tmp/f.md", name="explore")
      a, b = parallel(
          lambda: agent(f"impl auth, material: {findings.artifact}", name="impl-auth"),
          lambda: agent("impl db layer", name="impl-db", cli="codex"),
      )
      agent(f"verify {a.artifact} {b.artifact}", name="verify", cli="codex")
    """
    _resolve_scoped_team(None, required=True)
    import runpy

    script_path = str(Path(script).resolve())
    try:
        runpy.run_path(script_path, run_name="__main__")
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001 — surface script failures as CLI errors
        from .flow import FlowError

        if isinstance(exc, FlowError):
            _fail(str(exc))
        raise


@cli.group("pr")
def pr_cmd():
    """Pin a PR number on the team window's status bar."""


@pr_cmd.command("set")
@click.argument("number", type=int)
@_json_default_options
def pr_set_cmd(number: int, plain: bool):
    """Label the current team window with its PR number.

    Run right after ``gh pr create --draft`` — writes ``@hive-pr`` on the
    current tmux window and installs a per-window status-bar display derived
    from the global ``window-status-format`` / ``window-status-current-format``
    (the index position renders ``PR<n>``; user styling and padding are
    preserved). Idempotent — re-running replaces the stamp and re-derives
    the display.
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
            "run `hive pr set` from your team window"
        )
    tmux.set_window_option(window, "@hive-pr", str(number))
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
        click.echo(f"window {window} labeled @hive-pr={number} ({summary})")
    else:
        result: dict[str, object] = {"window": window, "pr": number, "display": display}
        click.echo(json.dumps(result, indent=2))


@pr_cmd.command("clear")
@_json_default_options
def pr_clear_cmd(plain: bool):
    """Clear the current team window's PR number stamp."""
    if not tmux.is_inside_tmux():
        _fail("must run inside tmux")
    window = tmux.get_current_window_target() or ""
    if not window:
        _fail("cannot determine current window")
    if not tmux.get_window_option(window, "hive-team"):
        _fail(
            "current window is not a hive team window (no @hive-team); "
            "run `hive pr clear` from your team window"
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
    if LEAD_AGENT_NAME in members:
        return members[LEAD_AGENT_NAME]
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
    return sorted(rows, key=lambda m: (m.get("name") != LEAD_AGENT_NAME, str(m.get("name"))))


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
                # For pr, present-and-empty IS the truth (hive pr clear);
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
        str(m.get("cli") or m.get("name") or "?") for m in members  # orch-first already
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
    return sorted(members, key=lambda m: (m.get("name") != LEAD_AGENT_NAME, str(m.get("name"))))


def _resume_members_into_live_team(
    snap: dict[str, object],
    win: dict[str, str],
    live: dict[str, tmux.PaneInfo],
    missing: list[dict[str, str]],
    fresh_members: frozenset[str] = frozenset(),
) -> dict[str, object]:
    """Spawn *missing* members back into a live team.

    Members with a saved session resume it; a member in *fresh_members*
    (no saved sessionId) is spawned fresh with the member bootstrap,
    following the live anchor's current cwd.
    """
    from . import layout as layout_mod

    team_name = str(snap["team"])
    window = win["window"]
    ws = win.get("workspace") or ""
    if not ws:
        _fail(f"live team window {window} has no workspace binding; not guessing")
    anchor_pane = _live_anchor_pane(live).pane_id
    snap_members = list(snap.get("members", []))  # type: ignore[arg-type]
    ordered = _resume_member_order(snap_members)
    snap_anchor_cwd = next(
        (str(m.get("cwd") or "") for m in ordered if m.get("sessionId") and m.get("cwd")),
        next((str(m.get("cwd") or "") for m in ordered if m.get("cwd")), ""),
    )
    spawned: list[tuple[str, str]] = []
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
                live_cwd = tmux.display_value(anchor_pane, "#{pane_current_path}") or ""
                cwd = live_cwd or snap_anchor_cwd
                session_kwargs: dict[str, str] = {"skill": "hive:hive"}
            else:
                _resume_progress(
                    f"resuming {name} ({m['cli']}) — replaying its session, this can take a while…"
                )
                cwd = str(m["cwd"])
                session_kwargs = {"session_id": str(m["sessionId"]), "session_mode": "resume", "skill": "none"}
            agent = Agent.spawn(
                name=name,
                team_name=team_name,
                target_pane=anchor_pane,
                cwd=cwd,
                split_horizontal=layout_mod.split_horizontal(window, count),
                split_size="50%",
                cli=str(m["cli"]),
                model=str(m.get("model", "")),
                workspace=ws,
                **session_kwargs,
            )
            # Track the pane the moment it exists: every later failure —
            # tagging, context, layout — must be able to kill it.
            spawned.append((str(m["name"]), agent.pane_id))
            _resume_progress(f"{m['name']} ready in {agent.pane_id}")
            hive_context.save_context_for_pane(
                agent.pane_id, team=team_name, workspace=ws, agent=str(m["name"])
            )
        layout_mod.apply_adaptive(window)
    except Exception as e:  # noqa: BLE001 — any failure must clean up the new panes only
        for _name, pane in spawned:
            tmux.kill_pane(pane)
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
    """Rebuild a dead team in a fresh window; transactional.

    Members with a saved session resume it; a member in *fresh_members*
    (no saved sessionId) is spawned fresh with the member bootstrap in the
    snapshot anchor's cwd.
    """
    from . import layout as layout_mod
    from . import resume as resume_store

    team_name = str(snap["team"])
    ws = str(snap.get("workspace") or "")
    if not ws:
        _fail("snapshot has no workspace; cannot resume")
    members = _resume_member_order(list(snap.get("members", [])))  # type: ignore[arg-type]
    # Anchor on a member whose session (and thus cwd) survived preflight —
    # a fresh member's recorded cwd may be a deleted worktree.
    anchor_cwd = next(
        (str(m.get("cwd") or "") for m in members if m.get("sessionId") and m.get("cwd")),
        next((str(m.get("cwd") or "") for m in members if m.get("cwd")), ""),
    )
    session_name = tmux.get_current_session_name() or "hive"
    window_name = team_name

    window, first_pane = tmux.new_window(session_name, name=window_name, cwd=anchor_cwd, detach=True)
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
                session_kwargs: dict[str, str] = {"skill": "hive:hive"}
            else:
                _resume_progress(
                    f"resuming {name} ({m['cli']}) — replaying its session, this can take a while…"
                )
                session_kwargs = {"session_id": str(m["sessionId"]), "session_mode": "resume", "skill": "none"}
            agent = Agent.spawn(
                name=name,
                team_name=team_name,
                target_pane=first_pane,
                cwd=(anchor_cwd if fresh else str(m["cwd"])),
                split_window=i > 0,
                split_horizontal=layout_mod.split_horizontal(window, i + 1),
                split_size="50%",
                cli=str(m["cli"]),
                model=str(m.get("model", "")),
                workspace=ws,
                **session_kwargs,
            )
            hive_context.save_context_for_pane(
                agent.pane_id, team=team_name, workspace=ws, agent=name
            )
            results.append((name, agent.pane_id))
            _resume_progress(f"{name} ready in {agent.pane_id}")
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

    Members with a saved agent session come back with it (claude ``-r``,
    codex ``resume``) in their original working directory; members whose
    snapshot has no saved session are respawned fresh with the member
    bootstrap, following the anchor's cwd. Run ``hive ls`` to see
    resumable handles.
    """
    if not handle:
        _fail("missing handle — run `hive ls` to see resumable teams")
    if not tmux.is_inside_tmux():
        _fail("hive resume requires a tmux session")
    from . import resume as resume_store
    from .agent import SUPPORTED_CLIS

    snap = resume_store.load_snapshot(handle)
    if snap is None:
        # Not an exact handle: try the 4-char short id shown by `hive ls`.
        snap = _resolve_short_resume_ref(handle)
    if snap is None:
        _fail(f"no usable snapshot for '{handle}' — run `hive ls` (missing or corrupt)")
    handle = str(snap["handle"])
    members = list(snap.get("members", []))
    names = [str(m.get("name") or "") for m in members]
    if not names or any(not n for n in names):
        _fail("snapshot roster is empty or has unnamed members — nothing to resume")
    roster = dict(zip(names, members))
    # A member without a saved session cannot be resumed, but the saved
    # context of the others is the whole point: bring it back fresh.
    fresh_members = frozenset(n for n, m in zip(names, members) if not m.get("sessionId"))
    if len(fresh_members) == len(names):
        _fail(
            "no member has a saved sessionId — nothing to resume with original "
            "context; start a fresh team instead (hive init)"
        )
    bad_cli = [n for n, m in zip(names, members) if m.get("cli") not in SUPPORTED_CLIS]
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


@cli.command()
@click.argument("to_agent")
@click.argument("body", required=False, default="")
@click.option("--artifact", default="", help="Artifact path for large payloads")
def send(to_agent: str, body: str, artifact: str):
    """Send a message to another agent — the only message verb.

    Threading is automatic: when the latest inbound message from the
    recipient is still unanswered, this send is recorded as its reply;
    otherwise it opens a new thread. Senders never handle msgIds.

    The recipient is an address, and every `from=` value on a received
    envelope is one — answer by copying it verbatim. A teammate is a bare
    name. A member of some team is `<team>.<member>` (how a Claude session
    outside tmux, e.g. the desktop app, reaches in; bare names work there
    too while unique across live teams — its message arrives as
    `from=ccd.<its name>`). A Claude session outside any team is
    `ccd.<name or title or pid>` (how a member reaches out).

    New-thread sends must keep `body` to a short summary and put details
    in `--artifact`; the body is rejected if longer than 500 chars, has
    3+ lines, contains fenced code, or starts markdown heading/list
    lines. A send that continues a thread is exempt.

    \b
    Delivery is binary and fire-and-forget: the native transport (claude
    daemon / codex daemon) either accepted the message — its runtime owns
    it from there — or the command exits non-zero with the transport
    error. Success prints nothing; there is nothing to poll afterwards.

    \b
    Examples:
      hive send dodo "review this diff" --artifact /tmp/diff.md
      hive send "ccd.PR review" "build is green"    # session by desktop title
      hive send dodo "see report" --artifact - <<'EOF'
      # Findings
      - item
      EOF
    """
    if to_agent.startswith("ccd."):
        _send_to_ccd_session(to_agent[4:], body, artifact)
        return
    explicit_team = ""
    if "." in to_agent:
        # A dot splits the address only when the prefix names a live team
        # (`honey.worker`); otherwise the address stays whole for
        # qualified-name resolution across pane tags.
        from .team import _find_team_window

        prefix, rest = to_agent.split(".", 1)
        if prefix and _find_team_window(prefix)[0]:
            explicit_team, to_agent = prefix, rest
    guest = None
    if not tmux.is_inside_tmux():
        # The root gate admitted this call because the process runs inside a
        # Claude session; that session is the sender and its inbox socket is
        # its identity.
        from .adapters import claude_sessions

        guest = claude_sessions.self_session()
        if guest is None:
            _fail(_TMUX_REQUIRED_MESSAGE)
    if guest is not None:
        team_name, t = _resolve_guest_send_target(to_agent, explicit_team)
        # The session NAME, never the title: a title may contain spaces, which
        # would break `<HIVE from=...>` attribute tokenization downstream. The
        # name addresses the session in `hive send ccd.<name>` just the same.
        sender = f"ccd.{guest.name}"
    else:
        if explicit_team:
            _fail(
                "team members address teammates by bare name; "
                "`<team>.<member>` is for a Claude session outside tmux"
            )
        team_name, t = _resolve_send_target_team(to_agent)
        sender = _resolve_sender(None)
    ws = _resolve_workspace(t, required=True)
    # Auto-anchor: the latest unanswered inbound from the recipient makes
    # this send its reply; senders never handle msgIds. Anything else is a
    # new thread and rides the root protocol. An unreadable bus (guest
    # sender, fresh workspace) just means no anchor — delivery still goes,
    # and a truly broken bus fails loudly in the send itself.
    import sqlite3

    reply_to = ""
    try:
        latest = bus.latest_inbound_send_event(ws, sender=sender, target=to_agent)
    except (OSError, sqlite3.Error):
        latest = None
    if latest is not None:
        candidate = str(latest.get("msgId") or "")
        if candidate and not bus.has_send_reply_to(
            ws, msg_id=candidate, sender=sender, target=to_agent
        ):
            reply_to = candidate
    if not reply_to:
        _validate_root_send_protocol(body, artifact)
    resolved_artifact = _resolve_artifact_path(artifact, workspace=ws)
    try:
        _request_send_payload(
            workspace=ws,
            team=t,
            sender_agent=sender,
            target_agent=to_agent,
            body=body,
            artifact=resolved_artifact,
            reply_to=reply_to,
            command_name="send",
        )
    except RuntimeError as exc:
        _fail(str(exc))
    # Fire-and-forget: success is silent (rule of silence). The bus row
    # carries the identity; `hive thread` reads it back.


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
    """Interrupt an agent's running turn.

    Aborts the turn over the member's own transport — addressed to its
    engine, not typed at its pane. Use when a peer is stuck in a tool
    loop or you need to abort a runaway action.

    \b
    Example:
      hive interrupt dodo
    """
    _, t = _resolve_scoped_team(None, required=True)
    assert t is not None
    try:
        agent = t.get(agent_name)
    except KeyError:
        _fail(f"member '{agent_name}' not found in team '{t.name}'")
        return
    try:
        agent.interrupt()
    except RuntimeError as e:
        _fail(str(e))
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
    # The script reads TMUX_PANE for its reply pane; inside a codex tool env
    # that variable is the shared daemon's (stripped) one, so hand it the
    # thread-resolved pane identity instead.
    pane = tmux.get_current_pane_id()
    if pane:
        os.environ["TMUX_PANE"] = pane
    # The script (and the post-popup sendback it generates) calls back into
    # hive for pane profile and claude-viewer questions. A bare `python3` is
    # whatever the pane's PATH resolves — usually not the interpreter hive is
    # installed in — so hand it this one.
    os.environ["HIVE_PYTHON"] = sys.executable
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
    # Thread-aware pane resolution: in a codex tool env TMUX_PANE is gone.
    reply_pane = tmux.get_current_pane_id() or ""
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
# [PROMPT], or `resume`/`fork`) is an interactive launch bound to the shared
# app-server daemon so hive can read its native runtime. Kept in sync with
# `codex --help`.
_CODEX_PASSTHROUGH_SUBCOMMANDS = (
    "exec", "e", "review", "login", "logout", "mcp", "plugin", "mcp-server",
    "app-server", "remote-control", "app", "completion", "update", "doctor",
    "sandbox", "debug", "apply", "a", "cloud", "exec-server", "features", "help",
)

# Non-interactive surfaces: --help/--version never start a session.
_CODEX_PASSTHROUGH_FLAGS = frozenset({"-h", "--help", "-V", "--version"})

# Global codex options that consume the following token as their value, so the
# subcommand scan does not mistake that value for the subcommand. `--opt=value`
# and `-Cvalue` are self-contained and handled separately.
_CODEX_VALUE_OPTS = frozenset({
    "-c", "--config", "-m", "--model", "-C", "--cd", "--remote",
    "--remote-auth-token-env", "--enable", "--disable", "-p", "--profile",
    "-a", "--ask-for-approval", "-s", "--sandbox",
})


def _codex_subcommand_index(args: list[str]) -> int | None:
    """Index of the first non-option token in `args` — the subcommand, if any.

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
            return i + 1 if i + 1 < len(args) else None
        if a.startswith("-"):
            i += 2 if (a in _CODEX_VALUE_OPTS and "=" not in a) else 1
            continue
        return i
    return None

def _codex_positional_after(args: list[str], sub_index: int) -> str | None:
    """First positional token after the subcommand (e.g. resume's SESSION_ID)."""
    i = sub_index + 1
    while i < len(args):
        a = args[i]
        if a == "--":
            return args[i + 1] if i + 1 < len(args) else None
        if a.startswith("-"):
            i += 2 if (a in _CODEX_VALUE_OPTS and "=" not in a) else 1
            continue
        return a
    return None


def _codex_opt_value(args: list[str], names: tuple[str, ...]) -> str | None:
    """Value of the first `--opt value` / `--opt=value` occurrence in `args`.

    A following token starting with `-` is the next flag, not this option's
    value: the option is read as bare (None) rather than swallowing it.
    """
    for i, a in enumerate(args):
        if a in names:
            nxt = args[i + 1] if i + 1 < len(args) else ""
            return nxt if nxt and not nxt.startswith("-") else None
        for name in names:
            if a.startswith(f"{name}=" if name.startswith("--") else name) and a != name:
                prefix = f"{name}=" if name.startswith("--") else name
                return a[len(prefix):]
    return None


def _pane_member_label(pane: str) -> str | None:
    """``<team>.<member>`` when the pane carries hive member tags, else None."""
    team = tmux.get_window_option(pane, "hive-team")
    agent = tmux.get_window_option(pane, "hive-agent")
    if team and agent:
        return f"{team}.{agent}"
    return None


def _codex_pane_thread_name(pane: str) -> str:
    """Thread name for launcher-minted threads (must be non-empty).

    Member panes get their member identity so the name means something
    anywhere codex surfaces it; non-member panes get a pane-derived
    placeholder.
    """
    return _pane_member_label(pane) or f"hive-{pane.replace('%', '') or 'pane'}"


def _exec_codex_managed(args: list[str]) -> None:
    """Replace this process with codex on the shared app-server daemon.

    Born-connected path for a user-launched codex: ensure the shared daemon,
    trust the working directory, bind this pane to a thread, then exec
    ``codex resume <threadId> --remote unix://<sock> --cd <cwd>`` so the TUI
    drives exactly the thread hive recorded for the pane — identity is the
    threadId, never the process environment.

    Thread binding by launch shape:
    - bare interactive launch: hive mints the thread (thread/start +
      name/set rollout flush) and execs a resume of it;
    - ``resume <id>``: that id becomes the pane's recorded thread;
    - ``fork <id>``: hive forks server-side (thread/fork), records the fork,
      and execs a resume of it;
    - ``resume`` picker / ``--last`` (thread unknowable up front): the pane's
      stale record is cleared and the launch runs remote-attached but
      unrecorded — degraded, no native runtime for that pane.

    Degrades to raw ``codex`` (embedded, status quo) whenever the managed path
    cannot apply: outside tmux, a management subcommand or --help/--version,
    an explicit ``--remote`` already given, or the daemon failing to bind. The
    caller never ends up worse than plain codex.
    """
    from .adapters import codex_app_server

    def _raw() -> None:
        os.execvp("codex", ["codex", *args])

    pane = os.environ.get("TMUX_PANE") or (tmux.get_current_pane_id() or "")
    if not pane or not tmux.is_inside_tmux():
        _raw()  # hive needs a tmux pane to bind a thread to
    sub_index = _codex_subcommand_index(args)
    sub = args[sub_index] if sub_index is not None else None
    if sub in _CODEX_PASSTHROUGH_SUBCOMMANDS:
        _raw()  # a management subcommand, not an interactive TUI launch
    if any(a in _CODEX_PASSTHROUGH_FLAGS for a in args):
        _raw()  # --help/--version never start a session
    if any(a == "--remote" or a.startswith("--remote=") for a in args):
        _raw()  # caller already chose an endpoint
    if not codex_app_server.spawn_daemon():
        _raw()  # daemon would not bind — fall back to embedded codex
    cwd = _codex_opt_value(args, ("--cd", "-C")) or os.getcwd()
    codex_app_server.ensure_dir_trusted(cwd)
    sock = codex_app_server.shared_socket_path()
    # -c check_for_update_on_startup=false mirrors the hive-spawned path so a
    # managed launch never drops the user into codex's npm self-update prompt.
    argv = ["codex", "-c", "check_for_update_on_startup=false", "--remote", f"unix://{sock}"]
    if not _codex_args_set_cwd(args):
        argv += ["--cd", cwd]

    if sub == "resume":
        sid = _codex_positional_after(args, sub_index)
        if sid:
            codex_app_server.write_pane_thread(pane, sid, cwd)
        else:
            # Picker / --last: the chosen thread is unknowable up front. A
            # stale record must not keep routing hive at the previous thread.
            codex_app_server.clear_pane_thread(pane)
        os.execvp("codex", argv + args)
    if sub == "fork":
        source = _codex_positional_after(args, sub_index)
        forked = (
            codex_app_server.fork_member_thread(
                source, name=_codex_pane_thread_name(pane)
            )
            if source
            else None
        )
        if forked:
            codex_app_server.write_pane_thread(pane, forked, cwd)
            rewritten = list(args)
            rewritten[sub_index] = "resume"
            rewritten[rewritten.index(source, sub_index + 1)] = forked
            os.execvp("codex", argv + rewritten)
        # No source id, or the fork RPC failed: let codex fork on its own —
        # remote-attached but unrecorded, so clear any stale pane record.
        codex_app_server.clear_pane_thread(pane)
        os.execvp("codex", argv + args)
    # Interactive launch — no subcommand, flags only, or a bare [PROMPT]:
    # mint the pane's thread so it is born with an identity hive can read,
    # deliver to, and resume. A trailing prompt rides `resume`'s own [PROMPT]
    # positional unchanged.
    minted = codex_app_server.start_member_thread(
        cwd,
        name=_codex_pane_thread_name(pane),
        model=_codex_opt_value(args, ("--model", "-m")) or "",
    )
    if minted:
        codex_app_server.write_pane_thread(pane, minted, cwd)
        os.execvp("codex", argv + ["resume", minted] + args)
    # Mint failed (daemon just died?): remote attach unrecorded — degraded,
    # and a stale record must not point hive at a thread this TUI won't run.
    codex_app_server.clear_pane_thread(pane)
    os.execvp("codex", argv + args)


def _codex_args_set_cwd(args: list[str]) -> bool:
    """True when the user already passed codex's cwd flag (-C / --cd, any form)."""
    return any(
        a == "--cd" or a.startswith("--cd=") or a.startswith("-C") for a in args
    )


@cli.command(
    "codex",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]},
    add_help_option=False,
)
@click.pass_context
def codex_cmd(ctx: click.Context):
    """Launch codex on the shared app-server daemon (hive-managed).

    Usually invoked through the `hcodex` launcher from `hive shell-init` rather
    than by hand; all arguments are forwarded to codex. Replaces the current process
    with codex and never returns on success.
    """
    _exec_codex_managed(list(ctx.args))


# claude subcommands that are not an interactive TUI launch: raw passthrough.
# Hidden subcommands are only recognized at argv[1], so args[0] is the one
# place a subcommand can sit.
_CLAUDE_PASSTHROUGH_SUBCOMMANDS = (
    "agents", "attach", "logs", "stop", "respawn", "rm", "mcp", "plugin",
    "config", "doctor", "update", "install", "migrate-installer",
    "setup-token", "api", "bg-spare", "bg-pty-host", "daemon", "help",
)

# Non-interactive surfaces: --help/--version never start a session.
_CLAUDE_PASSTHROUGH_FLAGS = frozenset({"-h", "--help", "-v", "--version"})

# Launch shapes the bg mapping cannot represent: headless print mode
# (rejected by --bg upstream), an explicit --bg the caller manages itself,
# and -c/--continue (which session it continues is unknowable up front).
_CLAUDE_RAW_MODE_FLAGS = frozenset({"-p", "--print", "--bg", "-c", "--continue"})


def _claude_resume_arg(args: list[str]) -> tuple[bool, str | None]:
    """(resume flag present, its value). ``-r``/``--resume`` take an optional
    value; a bare flag opens claude's picker."""
    for i, a in enumerate(args):
        if a in ("-r", "--resume"):
            if i + 1 < len(args) and not args[i + 1].startswith("-"):
                return True, args[i + 1]
            return True, None
        if a.startswith("--resume="):
            return True, a.split("=", 1)[1] or None
    return False, None


def _claude_pane_job_name(pane: str) -> str:
    """Job name for launcher-minted jobs (also the ledger row's label).

    Member panes get ``<team>.<member>`` so the native agent panel and
    ``claude agents`` rows say which member a job is — and so the view
    probe's title branch, which matches member names, can recognize the
    member when the human selects it there. Non-member panes get a
    pane-derived placeholder.
    """
    return _pane_member_label(pane) or f"hive-{pane.replace('%', '') or 'pane'}"


def _claude_attach_loop(job_id: str) -> None:
    """Replace this process with a watch loop keeping the pane attached to
    its bg job's engine. Never returns.

    A job-control shell (``set -m``) runs the loop so ``claude attach`` owns
    the tty foreground: the viewer gets the keyboard the human is typing on,
    and the pane's current command reads ``claude`` for anything that reads
    the pane as a display. Hive's own deliveries do not come through here —
    a member's keystrokes are addressed to its job, not to this pane.

    ``claude attach`` exits 0 both when the user detaches and when an engine
    respawn/upgrade kicks the viewer, so the loop reattaches after a 1s
    window the user can break out of with Ctrl-C (the interrupted ``sleep``
    ends the loop). A viewer killed by a signal (rc > 128) is also
    reattached; only a genuine error exit (1..128) that fails *fast* ends
    the loop instead of spinning on a removed job.
    """
    from .adapters import claude_bg

    script = (
        "set -m\n"
        "while :; do\n"
        "  t0=$(date +%s)\n"
        f"  claude attach {shlex.quote(job_id)}\n"
        "  rc=$?\n"
        "  if [ $rc -ge 1 ] && [ $rc -le 128 ] && "
        "[ $(( $(date +%s) - t0 )) -lt 5 ]; then\n"
        "    exit $rc\n"
        "  fi\n"
        f"  echo \"hive: viewer detached from job {shlex.quote(job_id)}; \"\\\n"
        "\"reattaching in 1s (Ctrl-C to stay detached)\" >&2\n"
        "  sleep 1 || exit 0\n"
        "done\n"
    )
    os.execve("/bin/sh", ["sh", "-c", script], claude_bg.bg_env())


def _exec_claude_managed(args: list[str]) -> None:
    """Run claude as a hive-managed background job with this pane attached.

    Born-managed path for a user-launched claude: mint (or rebind) a
    ``claude --bg`` job, record pane<->jobId, then hold the pane in an
    attach watch loop — the engine lives on claude's own supervisor, so the
    pane is a viewer, and the member survives the viewer dying.

    Job binding by launch shape:
    - bare interactive launch (flags / prompt only): mint a bg job with the
      forwarded flags and prompt;
    - ``--resume <jobId>``: rebind the pane to that job and attach (waking a
      parked engine — this is what spawn panes and resume hints run);
    - ``-r <sessionId> [--fork-session]``: mint a bg job resuming (or
      forking) that session;
    - ``-r``/``--resume`` with no value (picker): raw claude — the chosen
      session is unknowable up front.

    Degrades to raw ``claude`` (interactive TUI, unmanaged) whenever the
    managed path cannot apply: outside tmux, a management subcommand,
    --help/--version, headless/--bg/--continue shapes, or the bg spawn
    failing. The caller never ends up worse than plain claude.
    """
    from .adapters import claude_bg

    def _raw() -> None:
        os.execvp("claude", ["claude", *args])

    if args == ["channel-server"]:
        # Tombstone for the retired hive-channel plugin's MCP entry: exec'ing
        # this into claude would feed it a garbage subcommand every session.
        click.echo(
            "Error: the hive-channel plugin is retired (claude delivery now "
            "uses the session's own cross-session inbox). Remove it with: "
            "claude plugin uninstall hive-channel@hive",
            err=True,
        )
        sys.exit(1)
    pane = os.environ.get("TMUX_PANE") or ""
    if not pane or not os.environ.get("TMUX"):
        _raw()  # hive needs a real tmux pane to bind a job to
    if args and args[0] in _CLAUDE_PASSTHROUGH_SUBCOMMANDS:
        _raw()  # a management subcommand, not an interactive TUI launch
    if any(a in _CLAUDE_PASSTHROUGH_FLAGS for a in args):
        _raw()
    if any(a in _CLAUDE_RAW_MODE_FLAGS for a in args):
        _raw()

    resume_present, resume_val = _claude_resume_arg(args)
    if resume_present and not resume_val:
        _raw()  # picker: the chosen session is unknowable up front
    cwd = os.getcwd()

    if resume_val and claude_bg.looks_like_job_id(resume_val):
        engine = claude_bg.engine_session_for_job(resume_val)
        if engine is None and claude_bg.job_exists(resume_val):
            engine = claude_bg.ensure_engine(resume_val)
        if engine is not None or claude_bg.job_exists(resume_val):
            claude_bg.write_pane_job(
                pane, resume_val, engine.session_id if engine else "", cwd
            )
            _claude_attach_loop(resume_val)
            return  # unreachable in production (the loop execs); mocked in tests
        # Not a known job: fall through and treat the value as a session id.

    user_named = any(a == "--name" or a.startswith("--name=") for a in args)
    job_id = claude_bg.spawn_job(
        cwd=cwd,
        name="" if user_named else _claude_pane_job_name(pane),
        extra_args=list(args),
    )
    if not job_id:
        click.echo("hive: `claude --bg` failed; launching plain claude", err=True)
        _raw()
    engine = claude_bg.wait_engine_entry(job_id, timeout=10.0)
    claude_bg.write_pane_job(pane, job_id, engine.session_id if engine else "", cwd)
    _claude_attach_loop(job_id)


@cli.command(
    "claude",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]},
    add_help_option=False,
)
@click.pass_context
def claude_cmd(ctx: click.Context):
    """Launch claude as a hive-managed background job (hclaude launcher).

    Interactive launches run as `claude --bg` jobs with the pane attached as
    a viewer; management subcommands and non-interactive shapes pass through
    to plain claude. Does not return on the raw path; on the managed path it
    exits with the viewer loop's status.
    """
    _exec_claude_managed(list(ctx.args))


# --- grok managed launch ---

# grok subcommands that are not an interactive TUI launch: hive leaves these
# completely untouched (raw grok). Everything else — no arguments, flags only,
# or a bare [PROMPT] — is an interactive launch bound to the pane's leader
# daemon so hive can read its native runtime. A subcommand is always the first
# token; a prompt is the only other thing that can sit there.
_GROK_PASSTHROUGH_SUBCOMMANDS = (
    "agent", "completions", "dashboard", "doctor", "du", "export", "help",
    "inspect", "leader", "login", "logout", "mcp", "memory", "models", "plugin",
    "sessions", "setup", "trace", "update", "version", "worktree", "wrap",
)

# Non-interactive surfaces: --help/--version never start a session.
_GROK_PASSTHROUGH_FLAGS = frozenset({"-h", "--help", "-V", "--version"})


def _grok_opt_value(args: list[str], names: tuple[str, ...]) -> str | None:
    """Value of the first `--opt value` / `--opt=value` occurrence in `args`.

    A following token starting with `-` is the next flag, not this option's
    value: `--resume -m grok-4` resumes grok's own picker instead of recording
    `-m` as the pane's session id.
    """
    for i, a in enumerate(args):
        if a in names:
            nxt = args[i + 1] if i + 1 < len(args) else ""
            return nxt if nxt and not nxt.startswith("-") else None
        for name in names:
            if a.startswith(f"{name}="):
                return a[len(name) + 1:]
    return None


def _grok_launch_session(args: list[str]) -> tuple[str | None, bool]:
    """(session id this launch will run, whether hive must pass --session-id).

    grok mints nothing hive can observe, so hive names the session itself and
    records it beside the pane's socket. Two shapes already carry their own
    name: an explicit --session-id, and a plain --resume (grok rejects
    --session-id there unless --fork-session makes it the *new* fork's name).
    A --resume whose id hive cannot read leaves the pane unrecorded rather
    than making grok reject the launch.
    """
    explicit = _grok_opt_value(args, ("--session-id", "-s"))
    if explicit:
        return explicit, False
    if any(a == "--resume" or a.startswith("--resume=") for a in args):
        if "--fork-session" not in args:
            return _grok_opt_value(args, ("--resume",)), False
    return str(uuid.uuid4()), True


def _exec_grok_managed(args: list[str]) -> None:
    """Replace this process with grok, attached to a per-pane leader daemon.

    Born-connected path for a user-launched grok: start (or reuse) the pane's
    leader, then exec ``grok --leader --leader-socket <sock> --session-id <sid>
    <args>`` so hive can drive that session over a second leader client from
    the first turn — no restart and no transcript reverse-engineering.

    Degrades to raw ``grok`` whenever the managed path cannot apply: outside
    tmux, a management subcommand or --help/--version, or the leader failing
    to bind. The caller never ends up worse than plain grok.
    """
    from .adapters import grok_leader

    def _raw() -> None:
        os.execvp("grok", ["grok", *args])

    pane = os.environ.get("TMUX_PANE") or (tmux.get_current_pane_id() or "")
    if not pane or not tmux.is_inside_tmux():
        _raw()  # hive needs a tmux pane to bind a daemon to
    if args and args[0] in _GROK_PASSTHROUGH_SUBCOMMANDS:
        _raw()  # a management subcommand, not an interactive TUI launch
    if any(a in _GROK_PASSTHROUGH_FLAGS for a in args):
        _raw()  # --help/--version never start a session
    if not grok_leader.spawn_daemon(pane):
        # A raw grok drives whatever session it likes; leaving an earlier
        # record in place would have hive resolve that stale id as this pane's.
        grok_leader.pane_session_path(pane).unlink(missing_ok=True)
        click.echo("hive: grok leader did not start; launching plain grok", err=True)
        _raw()
    session_id, pass_flag = _grok_launch_session(args)
    argv = ["grok", "--leader", "--leader-socket", str(grok_leader.pane_socket_path(pane))]
    if pass_flag:
        argv += ["--session-id", session_id]
    if session_id:
        grok_leader.write_pane_session(pane, session_id, os.getcwd())
    argv += args
    os.execvp("grok", argv)


@cli.command(
    "grok",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True, "help_option_names": ["--help"]},
    add_help_option=False,
)
@click.pass_context
def grok_cmd(ctx: click.Context):
    """Launch grok attached to a per-pane leader daemon (hive-managed).

    Usually invoked through the `hgrok` launcher from `hive shell-init` rather
    than by hand; all arguments are forwarded to grok. Replaces the current
    process with grok and never returns on success.
    """
    _exec_grok_managed(list(ctx.args))


@cli.group("ccd")
def ccd_cmd():
    """Discover Claude Code sessions outside the team — the desktop app,
    another terminal — by their cross-session inbox registry.

    `hive ccd ls` lists the reachable sessions; messaging one is plain
    `hive send ccd.<name>` (name, desktop title, or pid).
    """


@ccd_cmd.command("ls")
def ccd_ls_cmd():
    """List the Claude Code sessions `hive send ccd.<name>` can reach.

    The same registry `/list-agents` reads: every live session that binds a
    cross-session inbox (Claude Code 2.1.224+). A session on an older CLI, or
    started in bare mode, has no inbox and is not listed. `title` is the
    desktop app's session title when one is set. A session that is really a
    live team member carries a `member` field with its `<team>.<agent>`
    address: message it over the bus, not here.
    """
    from .adapters import claude_sessions

    members = _live_member_pids()
    rows = []
    for s in claude_sessions.list_sessions():
        row = {"name": s.name, "title": s.title, "pid": s.pid, "kind": s.kind, "cwd": s.cwd}
        if s.pid in members:
            row["member"] = ".".join(members[s.pid])
        rows.append(row)
    click.echo(json.dumps({"sessions": rows}, indent=2, ensure_ascii=False))


@cli.command("resume-hint", hidden=True)
@click.argument("cli_name", type=click.Choice(["claude", "codex", "grok"]))
def resume_hint_cmd(cli_name: str):
    """Print a cd-ready resume command for the session this pane just ran.

    Called by the shell-init `hclaude`/`hcodex`/`hgrok` launchers after a managed
    launch exits: claude's own "Resume this session with" line omits the
    directory and codex/grok print none at all. Resolution rides hive's existing
    session truth only — codex reads the thread record its launch wrote (the
    record outlives the TUI), grok reads the session file its launch wrote,
    claude reads the pane's bg job record (the jobId outlives viewer and
    engine alike; `hive claude --resume <jobId>` reattaches and wakes it). A
    pane outside a hive team gets no hint; tracking arbitrary user panes is not
    this feature's job. Prints nothing and exits 0 on any failure: a hint must
    never break the wrapper.
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
    pane, _team, _agent = identity
    if cli_name == "codex":
        session_id = _pane_codex_session_id(pane)
        resume_cmd = "hive codex resume"
    elif cli_name == "grok":
        session_id = _pane_grok_session_id(pane)
        resume_cmd = "hive grok --resume"
    else:
        session_id = _pane_claude_job_id(pane)
        resume_cmd = "hive claude --resume"
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

    The team gate is shared by every CLI: any tmux user gets a thread record /
    session file from the managed launch, so a resolvable session alone must
    not qualify a pane for a hint — only hive team member panes are in scope.
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

    The pane's thread record (threadId == sessionId) is written at launch time
    and outlives the TUI — the same ``codex_app_server.session_id_for_pane``
    the sidecar uses. No record → None: no answer means no hint.
    """
    from .adapters import codex_app_server

    return codex_app_server.session_id_for_pane(pane)


def _pane_grok_session_id(pane: str) -> str | None:
    """This pane's grok session id, from the file the launch wrote.

    grok's session is named by hive at launch time and recorded beside the
    pane's leader socket, so the id survives the TUI exiting. No record → no
    hint.
    """
    from .adapters import grok_leader

    record = grok_leader.read_pane_session(pane)
    return record[0] if record else None


def _pane_claude_job_id(pane: str) -> str | None:
    """This pane's claude bg jobId, from the record its launch wrote.

    The record outlives the viewer and the engine alike (attach wakes a
    parked job with the same id), so the printed resume command works from
    any shell at any time. No record → no hint.
    """
    from .adapters import claude_bg

    return claude_bg.job_id_for_pane(pane)


_SHELL_INIT_POSIX = """\
# hive launchers — `hcodex` / `hclaude` / `hgrok` start a hive-connected codex /
# claude / grok in the current tmux pane (shared app-server daemon for codex,
# per-pane leader for grok, supervisor-hosted bg job for claude) and print a
# cd-ready resume hint when it exits. Outside tmux, and for management subcommands / non-interactive flags,
# they run the plain binary. Plain `codex` / `claude` / `grok` are never touched.
function hcodex {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hcodex: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is codex's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive codex "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # print a cd-ready resume hint for the session that just ended.
  hive resume-hint codex 2>/dev/null || true
  return $_hive_rc
}

function hclaude {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hclaude: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is claude's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive claude "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # claude's own resume hint omits the directory; print a cd-ready one.
  hive resume-hint claude 2>/dev/null || true
  return $_hive_rc
}

function hgrok {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hgrok: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is grok's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive grok "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # print a cd-ready resume hint for the session that just ended.
  hive resume-hint grok 2>/dev/null || true
  return $_hive_rc
}
"""

_SHELL_INIT_FISH = """\
# hive launchers — `hcodex` / `hclaude` / `hgrok` start a hive-connected codex /
# claude / grok in the current tmux pane (shared app-server daemon for codex,
# per-pane leader for grok, supervisor-hosted bg job for claude) and print a
# cd-ready resume hint when it exits. Outside tmux, and for management subcommands / non-interactive flags,
# they run the plain binary. Plain `codex` / `claude` / `grok` are never touched.
function hcodex
    if not type -q hive
        echo "hcodex: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # codex's own, never a fallback signal
    hive codex $argv
    set -l _hive_rc $status
    # print a cd-ready resume hint for the session that just ended.
    hive resume-hint codex 2>/dev/null
    return $_hive_rc
end

function hclaude
    if not type -q hive
        echo "hclaude: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # claude's own, never a fallback signal
    hive claude $argv
    set -l _hive_rc $status
    # claude's own resume hint omits the directory; print a cd-ready one.
    hive resume-hint claude 2>/dev/null
    return $_hive_rc
end

function hgrok
    if not type -q hive
        echo "hgrok: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # grok's own, never a fallback signal
    hive grok $argv
    set -l _hive_rc $status
    # print a cd-ready resume hint for the session that just ended.
    hive resume-hint grok 2>/dev/null
    return $_hive_rc
end
"""


@cli.command("shell-init")
@click.argument("shell", required=False, default="")
def shell_init_cmd(shell: str):
    """Print the `hcodex` / `hclaude` / `hgrok` launchers for your shell.

    Add to your shell rc; then `hcodex` / `hclaude` / `hgrok` start a
    hive-connected codex / claude / grok in the current tmux pane, while the
    plain `codex` / `claude` / `grok` stay untouched:

    \b
      # ~/.zshrc or ~/.bashrc
      eval "$(hive shell-init zsh)"
      # ~/.config/fish/config.fish
      hive shell-init fish | source

    Outside tmux, and for management subcommands and non-interactive flags,
    the launchers run the plain binary.
    """
    shell = (shell or os.path.basename(os.environ.get("SHELL", "") or "zsh")).strip()
    if shell == "fish":
        click.echo(_SHELL_INIT_FISH, nl=False)
    else:
        # zsh and bash share this syntax. The ksh-style `function name {` form
        # bypasses alias expansion of the name in BOTH shells, so a stray
        # alias cannot break the parse.
        click.echo(_SHELL_INIT_POSIX, nl=False)


# --- worktree pool ----------------------------------------------------------


def _worktree_context() -> dict:
    """Owner / integration context for worktree commands (pane-anchored, cwd-free)."""
    binding = _discover_tmux_binding()
    window = binding.get("tmuxWindow") or (
        (tmux.get_current_window_target() or "") if tmux.is_inside_tmux() else ""
    )
    team = binding.get("team", "")
    integration = (
        (tmux.get_window_option(window, "hive-integration-branch") if window else None) or None
    )
    owner = f"team:{team}" if team else "unbound"
    return {"owner": owner, "team": team, "integration": integration}


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


@worktree_cmd.command("set-base")
@click.argument("ref")
@_json_default_options
def worktree_set_base_cmd(ref: str, plain: bool):
    """Declare the team's integration branch (the base of every sub-PR).

    Run from the team window after creating and pushing the branch; every
    `hive worktree start` in this window afterwards resolves its base from
    it. REF must already resolve to a commit.
    """
    window = tmux.get_current_window_target() or ""
    team = (tmux.get_window_option(window, "hive-team") if window else None) or ""
    if not team:
        _fail("current window is not a hive team window (no @hive-team); run from your team window")
    from . import worktree as wt_mod

    try:
        anchor = wt_mod.repo_anchor(os.getcwd())
        oid = wt_mod.rev_parse(anchor, ref)
    except wt_mod.WorktreeError as e:
        _fail(str(e))
        raise AssertionError("unreachable")
    tmux.set_window_option(window, "@hive-integration-branch", ref)
    if plain:
        click.echo(f"team '{team}' integration branch set: {ref} ({oid[:12]})")
    else:
        click.echo(json.dumps({"team": team, "integrationBranch": ref, "oid": oid, "window": window}, indent=2))


@worktree_cmd.command("start")
@click.argument("feature")
@click.option(
    "--base",
    "base_ref",
    default=None,
    help="Base ref override (default: the window's integration branch from `hive worktree set-base`, else detected default branch)",
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
