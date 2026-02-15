"""CLI entry point for mission."""

from __future__ import annotations

import json
import os
import shutil
import sys

import click

from .team import Team, MISSION_HOME
from . import inbox as inbox_mod
from .inbox import Message


@click.group()
def cli():
    """Mission: tmux-based multi-agent collaboration for droid."""
    pass


# --- Team ---

@cli.command()
@click.argument("name")
@click.option("--desc", "-d", default="", help="Team description")
def create(name: str, desc: str):
    """Create a new team."""
    try:
        team = Team.create(name, description=desc)
        click.echo(f"Team '{name}' created.")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


# --- Agent ---

@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", required=True, help="Team name")
@click.option("--model", "-m", default="", help="Model ID")
@click.option("--prompt", "-p", default="", help="Initial prompt")
@click.option("--color", "-c", default="", help="Pane border color")
@click.option("--cwd", default="", help="Working directory")
def spawn(agent_name: str, team: str, model: str, prompt: str, color: str, cwd: str):
    """Spawn an agent in a team."""
    try:
        t = Team.load(team)
    except FileNotFoundError:
        click.echo(f"Team '{team}' not found. Create it first with: mission create {team}", err=True)
        sys.exit(1)

    try:
        agent = t.spawn(agent_name, model=model, prompt=prompt, color=color, cwd=cwd)
        click.echo(f"Agent '{agent_name}' spawned in pane {agent.pane_id}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@cli.command()
@click.argument("agent_name")
@click.argument("text")
@click.option("--team", "-t", required=True, help="Team name")
def send(agent_name: str, text: str, team: str):
    """Send a message to an agent's droid TUI."""
    t = Team.load(team)
    agent = t.get(agent_name)
    agent.send(text)
    click.echo(f"Sent to {agent_name}.")


@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", required=True, help="Team name")
@click.option("--lines", "-n", default=30, help="Lines to capture")
def capture(agent_name: str, team: str, lines: int):
    """Capture an agent's pane output."""
    t = Team.load(team)
    agent = t.get(agent_name)
    click.echo(agent.capture(lines))


@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", required=True, help="Team name")
def attach(agent_name: str, team: str):
    """Attach to an agent's tmux pane."""
    t = Team.load(team)
    agent = t.get(agent_name)
    os.execvp("tmux", ["tmux", "select-pane", "-t", agent.pane_id])


@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", required=True, help="Team name")
def interrupt(agent_name: str, team: str):
    """Interrupt an agent (press Escape)."""
    t = Team.load(team)
    agent = t.get(agent_name)
    agent.interrupt()
    click.echo(f"Interrupted {agent_name}.")


# --- Status ---

@cli.command()
@click.option("--team", "-t", required=True, help="Team name")
def status(team: str):
    """Show team status."""
    t = Team.load(team)
    s = t.status()
    click.echo(json.dumps(s, indent=2))


# --- Inbox ---

@cli.group()
def mail():
    """Inbox messaging between agents."""
    pass


@mail.command("send")
@click.argument("to_agent")
@click.argument("text")
@click.option("--team", "-t", required=True)
@click.option("--from", "from_agent", required=True, help="Sender name")
@click.option("--summary", "-s", default="")
def mail_send(to_agent: str, text: str, team: str, from_agent: str, summary: str):
    """Send a message to an agent's inbox."""
    t = Team.load(team)
    msg = Message(from_agent=from_agent, text=text, summary=summary)
    inbox_mod.send(t.inboxes_dir, to_agent, msg)
    click.echo(f"Message sent to {to_agent}.")


@mail.command("read")
@click.argument("agent_name")
@click.option("--team", "-t", required=True)
@click.option("--all", "show_all", is_flag=True, help="Show all messages")
def mail_read(agent_name: str, team: str, show_all: bool):
    """Read messages from an agent's inbox."""
    t = Team.load(team)
    if show_all:
        messages = inbox_mod.read_all(t.inboxes_dir, agent_name)
    else:
        messages = inbox_mod.read(t.inboxes_dir, agent_name)

    if not messages:
        click.echo("No messages.")
        return

    for m in messages:
        click.echo(f"[{m.timestamp}] from={m.from_agent}: {m.text[:200]}")
        if m.summary:
            click.echo(f"  summary: {m.summary}")


# --- Shutdown ---

@cli.command()
@click.option("--team", "-t", required=True, help="Team name")
@click.option("--agent", "-a", default=None, help="Specific agent (or all)")
@click.option("--force", is_flag=True, help="Force kill")
def shutdown(team: str, agent: str | None, force: bool):
    """Shutdown agent(s) or the entire team."""
    t = Team.load(team)
    if force:
        t.cleanup()
        click.echo(f"Team '{team}' killed.")
    elif agent:
        t.shutdown(agent)
        click.echo(f"Agent '{agent}' shutting down.")
    else:
        t.shutdown()
        click.echo(f"All agents shutting down.")


# --- Clean ---

@cli.command()
@click.argument("name")
def clean(name: str):
    """Clean up a team: kill all agent panes and remove team data."""
    try:
        t = Team.load(name)
        t.cleanup()
    except FileNotFoundError:
        pass

    # Remove team data
    teams_dir = MISSION_HOME / "teams" / name
    tasks_dir = MISSION_HOME / "tasks" / name
    for d in [teams_dir, tasks_dir]:
        if d.exists():
            shutil.rmtree(d)

    click.echo(f"Team '{name}' cleaned up.")
