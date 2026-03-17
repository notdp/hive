"""CLI entry point for hive."""

from __future__ import annotations

import json
import os
import shutil
import sys
import time
from pathlib import Path

import click

from . import bus
from . import context as hive_context
from . import tmux
from .agent import Agent
from .team import HIVE_HOME, Team, Terminal


def _discover_tmux_binding() -> dict[str, str]:
    if not tmux.is_inside_tmux():
        return {}
    current_pane = tmux.get_current_pane_id()
    current_session = tmux.get_current_session_name()
    if not current_pane:
        return {}
    root = HIVE_HOME / "teams"
    if not root.is_dir():
        return {}
    for path in sorted(root.iterdir()):
        if not path.is_dir():
            continue
        try:
            team = Team.load(path.name)
        except FileNotFoundError:
            continue
        if team.tmux_session and current_session and team.tmux_session != current_session:
            continue
        if team.lead_pane_id == current_pane:
            return {"team": team.name, "workspace": team.workspace, "agent": team.lead_name}
        for name, agent in team.agents.items():
            if agent.pane_id == current_pane:
                return {"team": team.name, "workspace": team.workspace, "agent": name}
        for name, terminal in team.terminals.items():
            if terminal.pane_id == current_pane:
                return {"team": team.name, "workspace": team.workspace, "agent": name, "role": "terminal"}
    return {}


def _default_team() -> str | None:
    discovered = _discover_tmux_binding()
    return os.environ.get("HIVE_TEAM_NAME") or discovered.get("team") or hive_context.load_current_context().get("team")


def _default_agent() -> str | None:
    discovered = _discover_tmux_binding()
    return os.environ.get("HIVE_AGENT_NAME") or discovered.get("agent") or hive_context.load_current_context().get("agent")


def _require_team(team: str | None) -> str:
    if team:
        return team
    click.echo("Error: --team/-t required (or set HIVE_TEAM_NAME, or run `hive use <team>`)", err=True)
    sys.exit(1)


def _resolve_sender(agent_name: str | None) -> str:
    return agent_name or _default_agent() or "orchestrator"


def _load_team(team: str) -> Team:
    try:
        return Team.load(team)
    except FileNotFoundError:
        click.echo(f"Error: team '{team}' not found", err=True)
        sys.exit(1)


def _fail(msg: str) -> None:
    click.echo(f"Error: {msg}", err=True)
    sys.exit(1)


def _resolve_workspace(workspace: str, team: Team | None = None, required: bool = False) -> str:
    if workspace:
        return workspace
    for env_name in ("HIVE_WORKSPACE", "CR_WORKSPACE"):
        env_workspace = os.environ.get(env_name, "")
        if env_workspace:
            return env_workspace
    current_context = hive_context.load_current_context()
    if current_context.get("workspace"):
        return current_context["workspace"]
    if team and team.workspace:
        return team.workspace
    if required:
        _fail("workspace is required (use --workspace, set HIVE_WORKSPACE, or run `hive use <team>`)")
    return ""


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


def _team_status_payload(t: Team) -> dict[str, object]:
    payload = t.status()
    ws = _resolve_workspace("", t, required=False)
    if ws:
        payload["publishedStatuses"] = bus.read_all_statuses(ws)
        bus.write_presence_snapshot(ws, payload)
    return payload


def _resolve_live_agent(t: Team | None, agent_name: str):
    if t is None:
        _fail("team is required for tmux-based Hive messaging")
    try:
        agent = t.get(agent_name)
    except KeyError:
        _fail(f"agent '{agent_name}' is not registered in team '{t.name}'")
    if not agent.is_alive():
        _fail(f"agent '{agent_name}' is not alive")
    return agent


def _format_hive_envelope(*, from_agent: str, to_agent: str, body: str, artifact: str = "") -> str:
    header = f"<HIVE from={from_agent} to={to_agent}"
    if artifact:
        header += f" artifact={artifact}"
    header += ">"
    payload = body.strip() if body.strip() else "(no message)"
    return f"{header}\n{payload}\n</HIVE>"


def _status_matches(payload: dict[str, object] | None, state: str, metadata: dict[str, str]) -> bool:
    if payload is None:
        return False
    if state and str(payload.get("state", "")) != state:
        return False
    payload_metadata = {str(k): str(v) for k, v in dict(payload.get("metadata", {})).items()}
    for key, value in metadata.items():
        if payload_metadata.get(key) != value:
            return False
    return True


@click.group()
def cli():
    """Hive: multi-agent collaboration for droid."""
    pass


def _gc_dead_teams() -> None:
    """Remove teams whose tmux session no longer exists."""
    root = HIVE_HOME / "teams"
    if not root.is_dir():
        return
    for path in sorted(root.iterdir()):
        if not path.is_dir():
            continue
        try:
            team = Team.load(path.name)
        except FileNotFoundError:
            continue
        if not team.is_tmux_alive():
            import shutil
            shutil.rmtree(path, ignore_errors=True)
            ctx = hive_context.load_current_context()
            if ctx.get("team") == team.name:
                hive_context.clear_current_context()


@cli.command("teams")
def teams_cmd():
    """List known Hive teams (auto-cleans dead tmux sessions)."""
    _gc_dead_teams()
    root = HIVE_HOME / "teams"
    rows = []
    if root.is_dir():
        for path in sorted(root.iterdir()):
            if not path.is_dir():
                continue
            try:
                team = Team.load(path.name)
            except FileNotFoundError:
                continue
            rows.append({
                "name": team.name,
                "workspace": team.workspace,
                "tmuxSession": team.tmux_session,
                "members": sorted({team.lead_name, *team.agents.keys()}),
            })
    click.echo(json.dumps(rows, indent=2, ensure_ascii=False))


@cli.command("current")
def current_cmd():
    """Show the persisted Hive CLI context, with tmux auto-discovery when no team is bound."""
    _gc_dead_teams()
    discovered = _discover_tmux_binding()
    if discovered.get("team"):
        pane_id = tmux.get_current_pane_id() or ""
        if pane_id:
            hive_context.save_context_for_pane(
                pane_id,
                team=discovered.get("team", ""),
                workspace=discovered.get("workspace", ""),
                agent=discovered.get("agent", ""),
            )
        click.echo(json.dumps(discovered, indent=2, ensure_ascii=False))
        return
    ctx = hive_context.load_current_context()
    if ctx.get("team"):
        click.echo(json.dumps(ctx, indent=2, ensure_ascii=False))
        return

    result: dict[str, object] = {**ctx, "team": None}

    if tmux.is_inside_tmux():
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
                    "role": p.role or ("agent" if p.command == "droid" else "terminal"),
                    "agent": p.agent,
                    "team": p.team,
                }
                for p in panes
            ],
            "paneCount": len(panes),
        }
        result["hint"] = "No team bound. Run `hive init` to create one from this tmux window."
    else:
        result["tmux"] = None
        result["hint"] = "Not inside tmux. Start a tmux session first, then run `hive init`."

    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.command("use")
@click.argument("team")
@click.option("--workspace", "-w", default="", help="Workspace path override")
@click.option("--agent", default="", help="Default agent name for standalone skill sessions")
def use_cmd(team: str, workspace: str, agent: str):
    """Persist a default Hive context so `/skill hive` can run without manual exports."""
    loaded = _load_team(team)
    resolved_workspace = workspace or loaded.workspace
    _remember_context(team=team, workspace=resolved_workspace, agent=agent or "orchestrator")
    click.echo(
        json.dumps(
            {
                "team": team,
                "workspace": resolved_workspace,
                "agent": agent or "orchestrator",
            },
            indent=2,
            ensure_ascii=False,
        )
    )


def _derive_agent_name(_title: str, index: int, seen: set[str]) -> str:
    """Derive a deterministic short agent name from pane order, not title."""
    fallback_words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot",
        "golf", "hotel", "india", "juliet", "kilo", "lima",
        "mike", "november", "oscar", "papa", "quebec", "romeo",
        "sierra", "tango", "uniform", "victor", "whiskey", "xray",
        "yankee", "zulu",
    ]
    fallback = fallback_words[index % len(fallback_words)]
    suffix = index // len(fallback_words)
    if suffix:
        fallback = f"{fallback}-{suffix + 1}"
    while fallback in seen:
        index += 1
        fallback = fallback_words[index % len(fallback_words)]
        suffix = index // len(fallback_words)
        if suffix:
            fallback = f"{fallback}-{suffix + 1}"
    seen.add(fallback)
    return fallback


@cli.command("init")
@click.option("--name", "-n", default="", help="Team name (default: tmux session name)")
@click.option("--workspace", "-w", default="", help="Workspace path (default: /tmp/hive-<session>-<window>/)")
@click.option("--notify/--no-notify", default=True, help="Push /skill hive + context to other panes")
def init_cmd(name: str, workspace: str, notify: bool):
    """Auto-discover tmux environment, create a team, register panes as agents."""
    if not tmux.is_inside_tmux():
        _fail("hive init requires a tmux session. Start tmux first.")

    _gc_dead_teams()

    existing = _discover_tmux_binding()
    if existing.get("team"):
        click.echo(json.dumps(existing, indent=2, ensure_ascii=False))
        return

    session_name = tmux.get_current_session_name() or "hive"
    window_index = tmux.get_current_window_index() or "0"
    window_target = tmux.get_current_window_target()

    team_name = name or session_name
    ws = workspace or f"/tmp/hive-{session_name}-{window_index}"

    panes = tmux.list_panes_full(window_target) if window_target else []
    current_pane = tmux.get_current_pane_id()

    try:
        t = Team.create(team_name, description=f"auto-init from tmux {session_name}:{window_index}", workspace=ws)
    except ValueError as e:
        _fail(str(e))

    ws_path = Path(ws)
    bus.init_workspace(ws_path)
    t.workspace = str(ws_path)
    t.tmux_session = session_name
    t.lead_name = "orchestrator"

    _remember_context(team=team_name, workspace=str(ws_path), agent="orchestrator")

    _SHELL_CMDS = {"zsh", "bash", "fish", "sh", "dash", "ksh", "tcsh", "csh"}

    seen_names: set[str] = {"orchestrator"}
    discovered = []
    agent_index = 0
    term_index = 0
    for pane in panes:
        is_current = pane.pane_id == current_pane
        if is_current:
            discovered.append({
                "paneId": pane.pane_id,
                "role": "lead",
                "name": "orchestrator",
                "command": pane.command,
                "isSelf": True,
            })
            continue

        is_agent = pane.command not in _SHELL_CMDS
        if is_agent:
            agent_name = _derive_agent_name(pane.title, agent_index, seen_names)
            agent_index += 1
            agent = Agent(
                name=agent_name,
                team_name=team_name,
                pane_id=pane.pane_id,
                cwd=os.getcwd(),
            )
            t.agents[agent_name] = agent
            tmux.tag_pane(pane.pane_id, "agent", agent_name, team_name)
            hive_context.save_context_for_pane(
                pane.pane_id, team=team_name, workspace=str(ws_path), agent=agent_name,
            )
            discovered.append({
                "paneId": pane.pane_id,
                "role": "agent",
                "name": agent_name,
                "command": pane.command,
                "isSelf": False,
            })
        else:
            term_index += 1
            term_name = f"term-{term_index}"
            while term_name in seen_names:
                term_index += 1
                term_name = f"term-{term_index}"
            seen_names.add(term_name)
            terminal = Terminal(name=term_name, pane_id=pane.pane_id)
            t.terminals[term_name] = terminal
            tmux.tag_pane(pane.pane_id, "terminal", term_name, team_name)
            discovered.append({
                "paneId": pane.pane_id,
                "role": "terminal",
                "name": term_name,
                "command": pane.command,
                "isSelf": False,
            })

    t.save()

    if notify:
        for agent in t.agents.values():
            agent.load_skill("hive")
            agent.send(
                f"You are '{agent.name}' in hive team '{team_name}'. "
                "Context is pre-bound. Hive messages will arrive inline as "
                "`<HIVE ...> ... </HIVE>` blocks. "
                "Use `hive who` to see teammates and `hive send <name> <message>` to reply."
            )

    result = {
        "team": team_name,
        "workspace": str(ws_path),
        "window": window_target,
        "panes": discovered,
    }
    click.echo(json.dumps(result, indent=2, ensure_ascii=False))


@cli.command()
@click.argument("name")
@click.option("--desc", "-d", default="", help="Team description")
@click.option("--workspace", "-w", default="", help="Workspace path to initialize")
@click.option("--reset-workspace", is_flag=True, help="Remove existing workspace before initialization")
@click.option("--state", "state_entries", multiple=True, help="Initial state KEY=VALUE (repeatable)")
def create(name: str, desc: str, workspace: str, reset_workspace: bool, state_entries: tuple[str, ...]):
    """Create a new team."""
    if state_entries and not workspace:
        _fail("--state requires --workspace")
    if reset_workspace and not workspace:
        _fail("--reset-workspace requires --workspace")
    try:
        t = Team.create(name, description=desc, workspace=workspace)
        if workspace:
            ws = Path(workspace).expanduser()
            if ws.exists() and reset_workspace:
                shutil.rmtree(ws)
            bus.init_workspace(ws)
            for key, value in _parse_entries(state_entries).items():
                (ws / "state" / key).write_text(value)
            t.workspace = str(ws)
            t.save()
            _remember_context(team=name, workspace=str(ws), agent="orchestrator")
        else:
            _remember_context(team=name, agent="orchestrator")
        click.echo(f"Team '{name}' created.")
        if workspace:
            click.echo(f"Workspace initialized: {Path(workspace).expanduser()}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@cli.command()
@click.argument("name")
@click.option("--workspace", "-w", default="", help="Workspace path to remove")
@click.option("--keep-workspace", is_flag=True, help="Keep workspace directory")
def delete(name: str, workspace: str, keep_workspace: bool):
    """Delete a team: kill agent panes + remove data."""
    team_workspace = ""
    try:
        t = Team.load(name)
        team_workspace = t.workspace
        t.cleanup()
    except FileNotFoundError:
        pass

    team_dir = HIVE_HOME / "teams" / name
    if team_dir.exists():
        shutil.rmtree(team_dir)
    legacy_tasks_dir = HIVE_HOME / "tasks" / name
    if legacy_tasks_dir.exists():
        shutil.rmtree(legacy_tasks_dir)

    resolved_workspace = workspace or team_workspace or os.environ.get("HIVE_WORKSPACE", "") or os.environ.get("CR_WORKSPACE", "")
    if resolved_workspace and not keep_workspace:
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
@click.option("--team", "-t", default=None, help="Team name (default: $HIVE_TEAM_NAME)")
@click.option("--model", "-m", default="", help="Model ID")
@click.option("--prompt", "-p", default="", help="Initial prompt (typed into TUI after startup)")
@click.option("--color", "-c", default="", help="Pane border color")
@click.option("--cwd", default="", help="Working directory")
@click.option("--skill", default="hive", help="Base skill to load after startup ('none' to skip)")
@click.option("--workflow", default="", help="Workflow skill to load after the base skill")
@click.option("--env", "-e", multiple=True, help="Extra env vars (KEY=VALUE, repeatable)")
def spawn(agent_name: str, team: str | None, model: str, prompt: str,
          color: str, cwd: str, skill: str, workflow: str, env: tuple[str, ...]):
    """Spawn an agent in a tmux pane."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    extra_env = _parse_entries(env) if env else {}
    try:
        agent = t.spawn(
            agent_name,
            model=model,
            prompt=prompt,
            color=color,
            cwd=cwd,
            skill=skill,
            workflow=workflow,
            extra_env=extra_env or None,
        )
        hive_context.save_context_for_pane(
            agent.pane_id,
            team=team,
            workspace=_resolve_workspace("", t, required=False),
            agent=agent_name,
        )
        _remember_context(team=team, workspace=_resolve_workspace("", t, required=False), agent="orchestrator")
        click.echo(f"Agent '{agent_name}' spawned in pane {agent.pane_id}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@cli.group()
def workflow():
    """Workflow skill helpers layered on top of the base Hive skill."""
    pass


@workflow.command("load")
@click.argument("agent_name")
@click.argument("workflow_name")
@click.option("--team", "-t", default=None, help="Team name")
@click.option("--prompt", default="", help="Optional prompt to send after loading the workflow")
def workflow_load(agent_name: str, workflow_name: str, team: str | None, prompt: str):
    """Load an additional workflow skill into an existing Hive agent pane."""
    team_name = _require_team(team or _default_team())
    t = _load_team(team_name)
    agent = t.get(agent_name)
    agent.load_skill(workflow_name)
    if prompt:
        agent.send(prompt)
    click.echo(f"Workflow '{workflow_name}' loaded into {agent_name}.")


@cli.command("wait-status")
@click.argument("agent_name")
@click.option("--team", "-t", default=None, help="Team name")
@click.option("--workspace", "-w", default="", help="Workspace path")
@click.option("--state", "expected_state", default="", help="Expected state")
@click.option("--meta", "metadata_entries", multiple=True, help="Required metadata KEY=VALUE")
@click.option("--timeout", default=600, type=int, show_default=True, help="Timeout in seconds")
@click.option("--interval", default=1.0, type=float, show_default=True, help="Poll interval in seconds")
def wait_status(
    agent_name: str,
    team: str | None,
    workspace: str,
    expected_state: str,
    metadata_entries: tuple[str, ...],
    timeout: int,
    interval: float,
):
    """Wait until an agent's published status matches the requested state/metadata."""
    team_name = team or _default_team()
    t = _load_team(team_name) if team_name else None
    ws = _resolve_workspace(workspace, t, required=True)
    metadata = _parse_entries(metadata_entries)

    start = time.time()
    deadline = start + timeout
    click.echo(f"Waiting for status from {agent_name}... [timeout: {timeout}s]")

    while True:
        payload = bus.read_status(ws, agent_name)
        if _status_matches(payload, expected_state, metadata):
            click.echo(json.dumps(payload, indent=2, ensure_ascii=False))
            return

        if t:
            team_status = t.status().get("agents", {})
            status_data = team_status.get(agent_name)
            if status_data is not None and not status_data.get("alive", False):
                click.echo(f"Error: {agent_name} is no longer alive", err=True)
                try:
                    click.echo(t.get(agent_name).capture(30), err=True)
                except Exception:
                    pass
                sys.exit(1)

        now = time.time()
        if now >= deadline:
            click.echo(f"Timed out after {timeout}s waiting for status from {agent_name}", err=True)
            sys.exit(1)

        elapsed = int(now - start)
        if elapsed > 0 and elapsed % 30 == 0:
            click.echo(f"  ... {elapsed}s elapsed")
        time.sleep(interval)


@cli.command("type")
@click.argument("agent_name")
@click.argument("text")
@click.option("--team", "-t", default=None, help="Team name")
def type_cmd(agent_name: str, text: str, team: str | None):
    """Send a prompt directly to an agent's session."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    t.get(agent_name).send(text)
    click.echo(f"Prompt sent to {agent_name}.")


@cli.command()
@click.option("--team", "-t", default=None)
def status(team: str | None):
    """Show team presence plus any published collaboration statuses."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    click.echo(json.dumps(_team_status_payload(t), indent=2, ensure_ascii=False))


@cli.command()
@click.option("--team", "-t", default=None)
def who(team: str | None):
    """Show who is present in the current hive team."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    click.echo(json.dumps(_team_status_payload(t), indent=2, ensure_ascii=False))


@cli.command("status-set")
@click.argument("state")
@click.argument("summary", required=False, default="")
@click.option("--agent", "agent_name", default=None, help="Agent name (default: $HIVE_AGENT_NAME or orchestrator)")
@click.option("--team", "-t", default=None, help="Team name")
@click.option("--workspace", "-w", default="", help="Workspace path")
@click.option("--meta", "metadata_entries", multiple=True, help="Metadata KEY=VALUE")
def status_set(
    state: str,
    summary: str,
    agent_name: str | None,
    team: str | None,
    workspace: str,
    metadata_entries: tuple[str, ...],
):
    """Publish an agent collaboration status."""
    team_name = team or _default_team()
    t = _load_team(team_name) if team_name else None
    ws = _resolve_workspace(workspace, t, required=True)
    path = bus.write_status(
        ws,
        _resolve_sender(agent_name),
        state=state,
        summary=summary,
        metadata=_parse_entries(metadata_entries),
    )
    click.echo(path)


@cli.command("status-show")
@click.option("--agent", "agent_name", default=None, help="Agent name")
@click.option("--team", "-t", default=None, help="Team name")
@click.option("--workspace", "-w", default="", help="Workspace path")
def status_show(agent_name: str | None, team: str | None, workspace: str):
    """Show published collaboration status for one agent or all agents."""
    team_name = team or _default_team()
    t = _load_team(team_name) if team_name else None
    ws = _resolve_workspace(workspace, t, required=True)
    if agent_name:
        payload = bus.read_status(ws, agent_name)
        if payload is None:
            _fail(f"no published status for agent '{agent_name}'")
        click.echo(json.dumps(payload, indent=2, ensure_ascii=False))
        return
    click.echo(json.dumps(bus.read_all_statuses(ws), indent=2, ensure_ascii=False))


@cli.command()
@click.argument("to_agent")
@click.argument("body", required=False, default="")
@click.option("--from", "from_agent", default=None, help="Sender agent name (default: $HIVE_AGENT_NAME or orchestrator)")
@click.option("--team", "-t", default=None, help="Team name")
@click.option("--workspace", "-w", default="", help="Workspace path")
@click.option("--artifact", default="", help="Artifact path for large payloads")
def send(
    to_agent: str,
    body: str,
    from_agent: str | None,
    team: str | None,
    workspace: str,
    artifact: str,
):
    """Send one tmux-delivered Hive envelope. Usage: hive send <to> \"<body>\" [--artifact path]"""
    team_name = team or _default_team()
    t = _load_team(team_name) if team_name else None
    _resolve_workspace(workspace, t, required=False)
    sender = _resolve_sender(from_agent)
    target = _resolve_live_agent(t, to_agent)
    resolved_artifact = str(Path(artifact).expanduser()) if artifact else ""
    if resolved_artifact and not Path(resolved_artifact).exists():
        _fail(f"artifact not found: {resolved_artifact}")
    envelope = _format_hive_envelope(
        from_agent=sender,
        to_agent=to_agent,
        body=body,
        artifact=resolved_artifact,
    )
    target.send(envelope)
    click.echo(f"Sent HIVE message to {to_agent}.")


@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", default=None)
@click.option("--lines", "-n", default=30)
def capture(agent_name: str, team: str | None, lines: int):
    """Capture an agent's pane output."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    click.echo(t.get(agent_name).capture(lines))


@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", default=None)
def interrupt(agent_name: str, team: str | None):
    """Interrupt an agent (Escape)."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    t.get(agent_name).interrupt()
    click.echo(f"Interrupted {agent_name}.")


# --- Terminal commands ---

def _resolve_terminal(t: Team, name: str) -> Terminal:
    if name not in t.terminals:
        _fail(f"terminal '{name}' not found in team '{t.name}'")
    terminal = t.terminals[name]
    if not terminal.is_alive():
        _fail(f"terminal '{name}' pane is no longer alive")
    return terminal


@cli.command("exec")
@click.argument("terminal_name")
@click.argument("command")
@click.option("--team", "-t", default=None, help="Team name")
def exec_cmd(terminal_name: str, command: str, team: str | None):
    """Send a command to a terminal pane (for interactive tools: htop, ssh, docker exec, etc.)."""
    team_name = _require_team(team or _default_team())
    t = _load_team(team_name)
    terminal = _resolve_terminal(t, terminal_name)
    tmux.send_keys(terminal.pane_id, command)
    click.echo(f"Sent to {terminal_name} ({terminal.pane_id}).")


@cli.group()
def terminal():
    """Manage terminal panes in the team."""
    pass


@terminal.command("add")
@click.argument("name", required=False, default="")
@click.option("--team", "-t", default=None, help="Team name")
@click.option("--pane", "pane_id", default="", help="Pane ID (default: current pane)")
def terminal_add(name: str, team: str | None, pane_id: str):
    """Register a pane as a terminal."""
    team_name = _require_team(team or _default_team())
    t = _load_team(team_name)
    resolved_pane = pane_id or tmux.get_current_pane_id()
    if not resolved_pane:
        _fail("cannot determine pane ID (are you in tmux?)")
    term_name = name or f"term-{len(t.terminals) + 1}"
    if term_name in t.terminals:
        _fail(f"terminal '{term_name}' already exists")
    t.terminals[term_name] = Terminal(name=term_name, pane_id=resolved_pane)
    tmux.tag_pane(resolved_pane, "terminal", term_name, team_name)
    t.save()
    click.echo(f"Terminal '{term_name}' registered ({resolved_pane}).")


@terminal.command("remove")
@click.argument("name")
@click.option("--team", "-t", default=None, help="Team name")
def terminal_remove(name: str, team: str | None):
    """Unregister a terminal pane."""
    team_name = _require_team(team or _default_team())
    t = _load_team(team_name)
    if name not in t.terminals:
        _fail(f"terminal '{name}' not found")
    terminal_obj = t.terminals.pop(name)
    if tmux.is_pane_alive(terminal_obj.pane_id):
        tmux.clear_pane_tags(terminal_obj.pane_id)
    t.save()
    click.echo(f"Terminal '{name}' removed.")
