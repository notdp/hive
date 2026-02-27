"""CLI entry point for mission."""

from __future__ import annotations

import json
import os
import re
import subprocess
import shutil
import sys
import time
from pathlib import Path

import click

from .team import Team, MISSION_HOME


def _default_team() -> str | None:
    return os.environ.get("MISSION_TEAM_NAME")


def _require_team(team: str | None) -> str:
    if team:
        return team
    click.echo("Error: --team/-t required (or set MISSION_TEAM_NAME)", err=True)
    sys.exit(1)


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
    env_workspace = os.environ.get("CR_WORKSPACE", "")
    if env_workspace:
        return env_workspace
    if team and team.workspace:
        return team.workspace
    if required:
        _fail("workspace is required (use --workspace or set CR_WORKSPACE)")
    return ""


def _parse_state_entries(entries: tuple[str, ...]) -> dict[str, str]:
    data: dict[str, str] = {}
    for entry in entries:
        if "=" not in entry:
            _fail(f"invalid --state entry '{entry}', expected KEY=VALUE")
        key, value = entry.split("=", 1)
        key = key.strip()
        if not key:
            _fail(f"invalid --state entry '{entry}', empty key")
        data[key] = value
    return data


def _read_state(workspace: str, key: str, required: bool = True) -> str:
    path = Path(workspace) / "state" / key
    if not path.exists():
        if required:
            _fail(f"missing state file: {path}")
        return ""
    return path.read_text().strip()


def _split_repo(repo: str) -> tuple[str, str]:
    if "/" not in repo:
        _fail(f"invalid repo '{repo}', expected owner/repo")
    owner, name = repo.split("/", 1)
    if not owner or not name:
        _fail(f"invalid repo '{repo}', expected owner/repo")
    return owner, name


def _gh(args: list[str], input_text: str | None = None) -> str:
    r = subprocess.run(
        ["gh", *args],
        input=input_text,
        capture_output=True,
        text=True,
        check=False,
    )
    if r.returncode != 0:
        stderr = r.stderr.strip()
        stdout = r.stdout.strip()
        _fail(stderr or stdout or f"gh command failed: {' '.join(args)}")
    return r.stdout.strip()


def _resolve_comment_context(
    workspace: str,
    repo: str,
    pr_number: str,
    pr_node_id: str,
    require_node_id: bool,
) -> tuple[str, str, str]:
    ws = _resolve_workspace(workspace, required=not (repo and pr_number and (pr_node_id or not require_node_id)))
    resolved_repo = repo or (_read_state(ws, "repo") if ws else "")
    resolved_pr = pr_number or (_read_state(ws, "pr-number") if ws else "")
    resolved_node = pr_node_id or (_read_state(ws, "pr-node-id", required=require_node_id) if ws else "")

    if not resolved_repo:
        _fail("repo is required (--repo or workspace state/repo)")
    if not resolved_pr:
        _fail("pr-number is required (--pr-number or workspace state/pr-number)")
    if require_node_id and not resolved_node:
        _fail("pr-node-id is required (--pr-node-id or workspace state/pr-node-id)")
    return resolved_repo, resolved_pr, resolved_node


@click.group()
def cli():
    """Mission: multi-agent collaboration for droid."""
    pass


# ── TeamCreate ──

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
            (ws / "state").mkdir(parents=True, exist_ok=True)
            (ws / "tasks").mkdir(parents=True, exist_ok=True)
            (ws / "results").mkdir(parents=True, exist_ok=True)
            (ws / "comments").mkdir(parents=True, exist_ok=True)
            for key, value in _parse_state_entries(state_entries).items():
                (ws / "state" / key).write_text(value)
            t.workspace = str(ws)
            t.save()
        click.echo(f"Team '{name}' created.")
        if workspace:
            click.echo(f"Workspace initialized: {Path(workspace).expanduser()}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


# ── TeamDelete ──

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

    team_dir = MISSION_HOME / "teams" / name
    if team_dir.exists():
        shutil.rmtree(team_dir)
    legacy_tasks_dir = MISSION_HOME / "tasks" / name
    if legacy_tasks_dir.exists():
        shutil.rmtree(legacy_tasks_dir)

    resolved_workspace = workspace or team_workspace or os.environ.get("CR_WORKSPACE", "")
    if resolved_workspace and not keep_workspace:
        ws = Path(resolved_workspace).expanduser()
        if ws.exists():
            shutil.rmtree(ws)
            click.echo(f"Workspace removed: {ws}")

    click.echo(f"Team '{name}' deleted.")


# ── Spawn ──

@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", default=None, help="Team name (default: $MISSION_TEAM_NAME)")
@click.option("--model", "-m", default="", help="Model ID")
@click.option("--prompt", "-p", default="", help="Initial prompt (typed into TUI after startup)")
@click.option("--color", "-c", default="", help="Pane border color")
@click.option("--cwd", default="", help="Working directory")
@click.option("--skill", default="mission", help="Skill to load after startup ('none' to skip)")
@click.option("--env", "-e", multiple=True, help="Extra env vars (KEY=VALUE, repeatable)")
def spawn(agent_name: str, team: str | None, model: str, prompt: str,
          color: str, cwd: str, skill: str, env: tuple[str, ...]):
    """Spawn an agent in a tmux pane."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    extra_env: dict[str, str] = {}
    for entry in env:
        if "=" in entry:
            k, v = entry.split("=", 1)
            extra_env[k] = v
    try:
        agent = t.spawn(agent_name, model=model, prompt=prompt, color=color,
                         cwd=cwd, skill=skill, extra_env=extra_env or None)
        click.echo(f"Agent '{agent_name}' spawned in pane {agent.pane_id}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@cli.command()
@click.argument("agent_name")
@click.argument("stage_tag")
@click.option("--team", "-t", default=None, help="Team name (default: $MISSION_TEAM_NAME)")
@click.option("--workspace", "-w", default="", help="Workspace path")
@click.option("--timeout", default=600, type=int, show_default=True, help="Timeout in seconds")
@click.option("--interval", default=1.0, type=float, show_default=True, help="Poll interval in seconds")
def wait(agent_name: str, stage_tag: str, team: str | None, workspace: str, timeout: int, interval: float):
    """Wait until workspace/results/{agent}-{stage}.done exists."""
    team_name = team or _default_team()
    t = _load_team(team_name) if team_name else None
    ws = _resolve_workspace(workspace, t, required=True)

    sentinel = Path(ws) / "results" / f"{agent_name}-{stage_tag}.done"
    result_file = Path(ws) / "results" / f"{agent_name}-{stage_tag}.md"

    start = time.time()
    deadline = start + timeout
    click.echo(f"Waiting for {agent_name} ({stage_tag})... [timeout: {timeout}s]")

    while True:
        if sentinel.exists():
            click.echo(f"{agent_name} ({stage_tag}): DONE")
            if result_file.exists():
                line_count = len(result_file.read_text().splitlines())
                click.echo(f"Result: {result_file} ({line_count} lines)")
            return

        if t:
            status_data = t.status().get("agents", {}).get(agent_name)
            if status_data is not None and not status_data.get("alive", False):
                click.echo(f"Error: {agent_name} is no longer alive", err=True)
                try:
                    click.echo(t.get(agent_name).capture(30), err=True)
                except Exception:
                    pass
                sys.exit(1)

        now = time.time()
        if now >= deadline:
            click.echo(f"Timed out after {timeout}s waiting for {agent_name} ({stage_tag})", err=True)
            if t:
                try:
                    click.echo(t.get(agent_name).capture(30), err=True)
                except Exception:
                    pass
            sys.exit(1)

        elapsed = int(now - start)
        if elapsed > 0 and elapsed % 30 == 0:
            click.echo(f"  ... {elapsed}s elapsed")

        time.sleep(interval)


# ── Type ──

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


# ── Status ──

@cli.command()
@click.option("--team", "-t", default=None)
def status(team: str | None):
    """Show team and agent status."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    click.echo(json.dumps(t.status(), indent=2))


# ── Capture ──

@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", default=None)
@click.option("--lines", "-n", default=30)
def capture(agent_name: str, team: str | None, lines: int):
    """Capture an agent's pane output."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    click.echo(t.get(agent_name).capture(lines))


# ── Interrupt ──

@cli.command()
@click.argument("agent_name")
@click.option("--team", "-t", default=None)
def interrupt(agent_name: str, team: str | None):
    """Interrupt an agent (Escape)."""
    team = _require_team(team or _default_team())
    t = _load_team(team)
    t.get(agent_name).interrupt()
    click.echo(f"Interrupted {agent_name}.")


@cli.group()
def comment():
    """GitHub PR comment operations."""
    pass


@comment.command("post")
@click.argument("body", required=False)
@click.option("--stdin", "from_stdin", is_flag=True, help="Read comment body from stdin")
@click.option("--workspace", "-w", default="", help="Workspace path")
@click.option("--repo", default="", help="Repo (owner/repo)")
@click.option("--pr-number", default="", help="PR number")
@click.option("--pr-node-id", default="", help="PR GraphQL node ID")
def comment_post(body: str | None, from_stdin: bool, workspace: str, repo: str, pr_number: str, pr_node_id: str):
    """Post comment and print node ID."""
    text = click.get_text_stream("stdin").read() if from_stdin else (body or "")
    if not text:
        _fail("empty body")

    _, _, node_id = _resolve_comment_context(
        workspace=workspace,
        repo=repo,
        pr_number=pr_number,
        pr_node_id=pr_node_id,
        require_node_id=True,
    )
    body_json = json.dumps(text)
    query = (
        "mutation { addComment(input: {subjectId: \""
        + node_id
        + "\", body: "
        + body_json
        + "}) { commentEdge { node { id } } } }"
    )
    result = json.loads(_gh(["api", "graphql", "-f", f"query={query}"]))
    created_id = result["data"]["addComment"]["commentEdge"]["node"]["id"]
    click.echo(created_id)


@comment.command("edit")
@click.argument("node_id")
@click.argument("body", required=False)
@click.option("--stdin", "from_stdin", is_flag=True, help="Read comment body from stdin")
def comment_edit(node_id: str, body: str | None, from_stdin: bool):
    """Edit an existing comment."""
    text = click.get_text_stream("stdin").read() if from_stdin else (body or "")
    if not text:
        _fail("empty body")

    body_json = json.dumps(text)
    query = (
        "mutation { updateIssueComment(input: {id: \""
        + node_id
        + "\", body: "
        + body_json
        + "}) { issueComment { id } } }"
    )
    _gh(["api", "graphql", "-f", f"query={query}"])
    click.echo(f"Updated {node_id}")


@comment.command("delete")
@click.argument("node_id")
def comment_delete(node_id: str):
    """Delete an existing comment."""
    query = f'mutation {{ deleteIssueComment(input: {{id: "{node_id}"}}) {{ clientMutationId }} }}'
    _gh(["api", "graphql", "-f", f"query={query}"])
    click.echo(f"Deleted {node_id}")


@comment.command("list")
@click.option("--workspace", "-w", default="", help="Workspace path")
@click.option("--repo", default="", help="Repo (owner/repo)")
@click.option("--pr-number", default="", help="PR number")
def comment_list(workspace: str, repo: str, pr_number: str):
    """List cross-review comments on a PR."""
    resolved_repo, resolved_pr, _ = _resolve_comment_context(
        workspace=workspace,
        repo=repo,
        pr_number=pr_number,
        pr_node_id="",
        require_node_id=False,
    )

    payload = json.loads(
        _gh(["pr", "view", str(resolved_pr), "--repo", resolved_repo, "--json", "comments"])
    )

    rows = []
    for c in payload.get("comments", []):
        body = c.get("body", "")
        if "<!-- cr-" not in body:
            continue
        marker_match = re.search(r"<!--\s*(cr-[a-z0-9-]+)\s*-->", body)
        rows.append({
            "id": c.get("id"),
            "marker": marker_match.group(1) if marker_match else "",
            "createdAt": c.get("createdAt"),
        })

    click.echo(json.dumps(rows, indent=2, ensure_ascii=False))


@comment.command("review-post")
@click.argument("body")
@click.argument("comments_json", required=False, default="[]")
@click.option("--workspace", "-w", default="", help="Workspace path")
@click.option("--repo", default="", help="Repo (owner/repo)")
@click.option("--pr-number", default="", help="PR number")
def comment_review_post(body: str, comments_json: str, workspace: str, repo: str, pr_number: str):
    """Post a PR review with optional inline comments JSON."""
    resolved_repo, resolved_pr, _ = _resolve_comment_context(
        workspace=workspace,
        repo=repo,
        pr_number=pr_number,
        pr_node_id="",
        require_node_id=False,
    )
    owner, repo_name = _split_repo(resolved_repo)

    try:
        comments = json.loads(comments_json)
        if not isinstance(comments, list):
            _fail("comments_json must be a JSON array")
    except json.JSONDecodeError:
        _fail("invalid comments_json, expected JSON array string")

    head_ref = ""
    try:
        head_ref = json.loads(
            _gh(["pr", "view", str(resolved_pr), "--repo", resolved_repo, "--json", "headRefOid"])
        ).get("headRefOid", "")
    except SystemExit:
        raise
    except Exception:
        head_ref = ""

    if not head_ref:
        pull_data = json.loads(_gh(["api", f"repos/{owner}/{repo_name}/pulls/{resolved_pr}"]))
        head_ref = pull_data.get("head", {}).get("sha", "")
    if not head_ref:
        _fail("failed to resolve PR head commit")

    payload = {
        "commit_id": head_ref,
        "body": body,
        "event": "COMMENT",
        "comments": comments,
    }
    result = json.loads(
        _gh(
            ["api", f"/repos/{owner}/{repo_name}/pulls/{resolved_pr}/reviews", "--method", "POST", "--input", "-"],
            input_text=json.dumps(payload),
        )
    )
    click.echo(f"Created review {result.get('id', '?')}")
