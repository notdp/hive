"""Team: a tmux window with a group of agents."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field

from . import tmux
from .agent import Agent
from .agent_cli import member_role_for_pane

HIVE_HOME = __import__("pathlib").Path(os.environ.get("HIVE_HOME", str(__import__("pathlib").Path.home() / ".hive")))
LEAD_AGENT_NAME = "orch"
_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."


def validate_team_name(name: str) -> str:
    """Why *name* cannot be a team name, or "" when it can."""
    if name == "ccd":
        return (
            f"team name '{name}' is invalid: 'ccd' is the reserved send "
            "address for Claude sessions outside any team"
        )
    if "." in name:
        return (
            f"team name '{name}' is invalid: dots separate send-address "
            "segments (`<team>.<member>`), so a team name must be dot-free"
        )
    from . import registry

    if registry.entry_path(name) is None:
        return f"team name '{name}' is invalid: not a safe registry name"
    return ""

@dataclass
class Team:
    name: str
    description: str = ""
    workspace: str = ""
    lead_name: str = LEAD_AGENT_NAME
    agents: dict[str, Agent] = field(default_factory=dict)
    created_at: float = field(default_factory=time.time)
    lead_pane_id: str = ""
    lead_session_id: str | None = None
    tmux_session: str = ""
    tmux_window: str = ""
    tmux_window_id: str = ""
    member_groups: dict[str, str] = field(default_factory=dict)

    # --- Window-level tmux options ---

    def _write_window_options(self) -> None:
        target = self.tmux_window
        if not target:
            return
        tmux.configure_hive_window(target)
        tmux.set_window_option(target, "@hive-team", self.name)
        tmux.set_window_option(target, "@hive-workspace", self.workspace)
        if self.description:
            tmux.set_window_option(target, "@hive-desc", self.description)
        tmux.set_window_option(target, "@hive-created", str(self.created_at))
    # --- Lifecycle ---

    @classmethod
    def create_for_window(
        cls,
        name: str,
        *,
        window_target: str,
        lead_pane_id: str = "",
        lead_name: str = LEAD_AGENT_NAME,
        description: str = "",
        cwd: str = "",
        workspace: str = "",
        tag_lead: bool = True,
    ) -> Team:
        """Create a team bound to *window_target* (not necessarily the focused
        window).

        ``create()`` binds to the currently-focused tmux window, which is wrong
        after a ``break_pane`` moves the lead pane to a fresh window while the
        client still views the origin. ``create_for_window`` takes the final
        window explicitly so callers can break out first, then bind the team
        where the pane actually landed — team identity must follow the final
        window (Bug A).
        """
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)
        error = validate_team_name(name)
        if error:
            raise ValueError(error)
        from . import registry

        if registry.load(name) is not None:
            # The registry is the name authority: a headless or detached
            # team owns its name (its engines may still be running) until
            # `hive delete` releases it. Never silently clobbered.
            raise ValueError(
                f"team '{name}' already exists in the registry "
                f"(hive delete {name} releases the name)"
            )

        existing_team = tmux.get_window_option(window_target, "hive-team") if window_target else None
        if existing_team:
            raise ValueError(f"Team '{existing_team}' already exists in this window")

        resolved_cwd = cwd or os.getcwd()
        team = cls(name=name, description=description, workspace=workspace, lead_name=lead_name)

        team.lead_pane_id = lead_pane_id or tmux.get_current_pane_id() or ""
        from .agent import detect_current_session_id
        team.lead_session_id = detect_current_session_id(resolved_cwd, pane_id=team.lead_pane_id)
        team.tmux_session = (
            window_target.split(":")[0] if ":" in window_target else (tmux.get_current_session_name() or "")
        )
        team.tmux_window = window_target
        team.tmux_window_id = tmux.get_window_id(window_target) or ""
        if tag_lead and team.lead_pane_id:
            tmux.tag_pane(team.lead_pane_id, member_role_for_pane(team.lead_pane_id), team.lead_name, name)

        team._write_window_options()
        return team

    @classmethod
    def create(
        cls,
        name: str,
        description: str = "",
        cwd: str = "",
        workspace: str = "",
    ) -> Team:
        """Create a new team in the currently-focused tmux window."""
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)
        return cls.create_for_window(
            name,
            window_target=tmux.get_current_window_target() or "",
            lead_pane_id=tmux.get_current_pane_id() or "",
            description=description,
            cwd=cwd,
            workspace=workspace,
            tag_lead=True,
        )

    @classmethod
    def load(cls, name: str, *, prefer_pane: str = "") -> Team:
        """Load a team: registry entry for identity and roster, tmux for display.

        The registry is the authoritative record — a team with an entry loads
        even when no tmux window renders it (members then have no pane
        binding). The tmux window, when one claims the team, binds panes onto
        roster members and contributes display-only metadata; a pane-tagged
        member missing from the registry still loads (union), so a team
        predating the registry writers keeps working.
        When *prefer_pane* is given, its window is preferred when multiple
        windows claim the same team name.
        """
        from . import registry

        snap = registry.load(name)
        hint = prefer_pane or tmux.get_current_pane_id() or ""
        window_target, window_data = _find_team_window(name, prefer_pane=hint)
        if snap is None and not window_target:
            raise FileNotFoundError(f"Team '{name}' not found")

        team = cls(
            name=name,
            description=window_data.get("desc", ""),
            workspace=(str(snap.get("workspace") or "") if snap else "")
            or window_data.get("workspace", ""),
            created_at=float(
                (str(snap.get("createdAt")) if snap and snap.get("createdAt") else "")
                or window_data.get("created")
                or 0
            ),
            tmux_session=window_target.split(":")[0] if ":" in window_target else "",
            tmux_window=window_target,
            tmux_window_id=window_data.get("window_id", ""),
        )

        if snap is not None:
            for row in snap.get("members", []):
                member = str(row.get("name") or "")
                if not member:
                    continue
                team.agents[member] = Agent(
                    name=member,
                    team_name=name,
                    pane_id="",
                    cli=str(row.get("cli") or "") or "claude",
                    cwd=str(row.get("cwd") or ""),
                    model=str(row.get("model") or ""),
                    session_id=str(row.get("sessionId") or "") or None,
                )

        panes = tmux.list_panes_full(window_target) if window_target else []
        for pane in panes:
            if pane.team != name:
                continue
            if pane.role == "agent":
                if pane.agent and pane.group:
                    team.member_groups[pane.agent] = pane.group
                from .agent_cli import AGENT_CLI_NAMES, detect_profile_for_pane, normalize_command
                resolved_cli = pane.cli or normalize_command(pane.command)
                if resolved_cli not in AGENT_CLI_NAMES:
                    profile = detect_profile_for_pane(pane.pane_id)
                    resolved_cli = profile.name if profile else "claude"
                agent = Agent(
                    name=pane.agent,
                    team_name=name,
                    pane_id=pane.pane_id,
                    cli=resolved_cli,
                    cwd=tmux.display_value(pane.pane_id, "#{pane_current_path}") or "",
                )
                if resolved_cli == "codex":
                    # A codex member's session id IS its threadId on the
                    # shared app-server daemon, recorded per pane at
                    # spawn/launch time.
                    from .adapters.codex_app_server import thread_id_for_pane
                    agent.session_id = thread_id_for_pane(pane.pane_id)
                elif resolved_cli == "claude":
                    # A claude member's durable identity is its bg jobId,
                    # recorded per pane at spawn/launch time — resume
                    # wakes the job, so the jobId is what snapshots and
                    # resume flows carry.
                    from .adapters.claude_bg import job_id_for_pane
                    agent.session_id = job_id_for_pane(pane.pane_id)
                registered = team.agents.get(pane.agent)
                if registered is not None:
                    # A live pane is fresher than the registry row for
                    # display-derived fields, but the recorded engine
                    # identity survives a pane whose records were wiped.
                    agent.session_id = agent.session_id or registered.session_id
                    agent.model = agent.model or registered.model
                team.agents[pane.agent] = agent

        return team

    def save(self) -> None:
        """Write team state to tmux options (window + pane level)."""
        self._write_window_options()

    def lead_agent(self) -> Agent | None:
        if not self.lead_pane_id:
            return None
        return Agent(
            name=self.lead_name,
            team_name=self.name,
            pane_id=self.lead_pane_id,
            cli=tmux.get_pane_option(self.lead_pane_id, "hive-cli") or "",
            cwd=tmux.display_value(self.lead_pane_id, "#{pane_current_path}") or os.getcwd(),
            session_id=self.lead_session_id,
        )

    # --- Agent management ---

    def spawn(
        self,
        name: str,
        model: str = "",
        prompt: str = "",
        cwd: str = "",
        skill: str = "hive",
        extra_env: dict[str, str] | None = None,
        cli: str = "claude",
    ) -> Agent:
        """Spawn a new agent in the team."""
        if name == "flow":
            raise ValueError("'flow' is the flow runner's reserved mailbox address, not a member name")
        if name in self.agents:
            raise ValueError(f"Agent '{name}' already exists in team '{self.name}'")
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        is_first = len(self.agents) == 0
        from . import layout
        # The team's own window, never the caller's focused one — a spawn
        # issued from another window must land and re-tile where the team
        # lives (kill already resolves the same way).
        window_for_split = self.tmux_window or tmux.get_current_window_target() or ""
        if is_first:
            target = self.lead_pane_id or tmux.get_current_pane_id() or ""
            split_horizontal = layout.split_horizontal(window_for_split, 2)
        else:
            last_agent = list(self.agents.values())[-1]
            target = last_agent.pane_id
            split_horizontal = False
        split_size = "50%"

        agent = Agent.spawn(
            name=name,
            team_name=self.name,
            target_pane=target,
            model=model,
            prompt=prompt,
            cwd=cwd or os.getcwd(),
            is_first=is_first,
            split_horizontal=split_horizontal,
            split_size=split_size,
            skill=skill,
            extra_env=extra_env,
            cli=cli,
        )

        tmux.tag_pane(agent.pane_id, "agent", name, self.name, cli=cli)
        self.agents[name] = agent

        window_target = self.tmux_window or tmux.get_current_window_target()
        if window_target:
            tmux.configure_hive_window(window_target)
            from . import layout
            layout.apply_adaptive(window_target)

        return agent

    def get(self, name: str) -> Agent:
        lead = self.lead_agent()
        if lead is not None and name == lead.name:
            return lead
        if name not in self.agents:
            raise KeyError(f"Agent '{name}' not found")
        return self.agents[name]

    def status(self) -> dict:
        """Get team status."""
        members: list[dict[str, object]] = []
        lead = self.lead_agent()
        if lead is not None:
            row = {
                "name": lead.name,
                "role": member_role_for_pane(lead.pane_id),
                "pane": lead.pane_id,
            }
            group = self.member_groups.get(lead.name, "")
            if group:
                row["group"] = group
            members.append(row)
        for name in sorted(self.agents):
            row = {
                "name": name,
                "role": "agent",
                "pane": self.agents[name].pane_id,
            }
            group = self.member_groups.get(name, "")
            if group:
                row["group"] = group
            members.append(row)
        return {
            "name": self.name,
            "description": self.description,
            "workspace": self.workspace,
            "tmuxSession": self.tmux_session,
            "tmuxWindow": self.tmux_window,
            "members": members,
        }

    def cleanup(self) -> None:
        """Kill all agent panes (not the session itself if in-place)."""
        for agent in self.agents.values():
            agent.kill()
        if self.lead_pane_id and tmux.is_pane_alive(self.lead_pane_id):
            tmux.clear_pane_tags(self.lead_pane_id)


def _window_has_live_team_members(window_target: str, team_name: str) -> bool:
    """True when *window_target* still hosts a live pane tagged as a member of
    *team_name*.

    A window with live members is a real team, not a stale leftover — duplicate
    resolution must never strip its tags, even when another window claims the
    same name. Callers destroy window options on False, so a failed tmux
    listing (unknown) conservatively counts as live: only a successful listing
    can prove a window stale.
    """
    panes = tmux.list_panes_full_or_none(window_target)
    if panes is None:
        return True
    return any(p.team == team_name and (p.agent or p.role) for p in panes)


def _find_team_window(name: str, *, prefer_pane: str = "") -> tuple[str, dict[str, str]]:
    """Find the tmux window that hosts team *name* by scanning window options.

    When multiple windows claim the same team name (e.g. after a window
    move/reorder leaves stale tags), the window containing *prefer_pane*
    wins.  If *prefer_pane* is not supplied we fall back to the window
    that actually has panes tagged for the team.  Provably-stale duplicates
    (no live member panes) get their ``@hive-team`` tag stripped; live
    duplicates are preserved so two colliding teams never lose their tags.
    """
    r = tmux._run([
        "list-windows", "-a", "-F",
        "#{session_name}:#{window_index}\t#{window_id}\t#{@hive-team}\t#{@hive-workspace}\t#{@hive-desc}\t#{@hive-created}",
    ], check=False)

    candidates: list[tuple[str, dict[str, str]]] = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) == 5:
            parts.insert(1, "")
        while len(parts) < 6:
            parts.append("")
        if parts[2] == name:
            candidates.append((parts[0], {
                "window_id": parts[1],
                "workspace": parts[3],
                "desc": parts[4],
                "created": parts[5],
            }))

    if not candidates:
        return "", {}
    if len(candidates) == 1:
        return candidates[0]

    # Multiple windows claim this team — resolve the conflict.
    # 1) Prefer the window that contains *prefer_pane*.
    if prefer_pane:
        pane_window = tmux.get_pane_window_target(prefer_pane)
        if pane_window:
            for wt, data in candidates:
                if wt == pane_window:
                    _gc_stale_team_windows(name, keep=wt, all_windows=[c[0] for c in candidates])
                    return wt, data

    # 2) Prefer the window that has panes actually tagged for this team.
    for wt, data in candidates:
        if _window_has_live_team_members(wt, name):
            _gc_stale_team_windows(name, keep=wt, all_windows=[c[0] for c in candidates])
            return wt, data

    # 3) Fall back to first match (shouldn't normally happen).
    return candidates[0]


def _gc_stale_team_windows(name: str, *, keep: str, all_windows: list[str]) -> None:
    """Strip @hive-team (and sibling options) from *provably stale* duplicate
    windows of *name*.

    A window that still hosts live member panes is left untouched: two live
    teams that collide on a name must both survive so neither loses its routing
    tags. ``hive doctor`` surfaces such collisions for manual repair.
    """
    for wt in all_windows:
        if wt == keep:
            continue
        if _window_has_live_team_members(wt, name):
            continue
        for key in ("hive-team", "hive-workspace", "hive-desc", "hive-created", "hive-peers"):
            tmux.clear_window_option(wt, f"@{key}")


def duplicate_team_bindings() -> list[dict[str, object]]:
    """Report tmux windows that collide on the same ``@hive-team`` name.

    Bug A could leave two live teams tagged with one name across different
    windows. This scans all windows, groups by team, and returns every group
    with more than one window — including each window's id, workspace, and live
    member panes — so ``hive doctor`` can surface the collision. Detection only:
    retagging a live team can break sidecar identity / pane context / pending
    sends, so repair is left to a human.
    """
    r = tmux._run([
        "list-windows", "-a", "-F",
        "#{session_name}:#{window_index}\t#{window_id}\t#{@hive-team}\t#{@hive-workspace}",
    ], check=False)

    by_team: dict[str, list[dict[str, object]]] = {}
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t")
        while len(parts) < 4:
            parts.append("")
        window, window_id, team, workspace = parts[0], parts[1], parts[2], parts[3]
        if not team:
            continue
        members = [
            {"name": p.agent, "pane": p.pane_id, "group": p.group}
            for p in tmux.list_panes_full(window)
            if p.team == team and (p.agent or p.role)
        ]
        by_team.setdefault(team, []).append({
            "tmuxWindow": window,
            "windowId": window_id,
            "workspace": workspace,
            "liveMembers": members,
        })

    duplicates: list[dict[str, object]] = []
    for team, windows in by_team.items():
        if len(windows) > 1:
            duplicates.append({
                "team": team,
                "windows": windows,
                "repair": "manual: two windows claim this team; do not auto-retag a live team",
            })
    return duplicates


def list_teams() -> list[dict[str, str]]:
    """List all teams: registry entries unioned with tmux-tagged windows.

    A registry entry lists its team whether or not a window renders it; a
    window row fills in (or contributes teams predating the registry).
    """
    from . import registry

    by_name: dict[str, dict[str, str]] = {}
    for entry in registry.list_entries():
        team = str(entry.get("team") or "")
        if entry.get("corrupt") or not team:
            continue
        by_name[team] = {
            "name": team,
            "tmuxWindow": "",
            "tmuxSession": "",
            "workspace": str(entry.get("workspace") or ""),
        }

    r = tmux._run([
        "list-windows", "-a", "-F",
        "#{session_name}:#{window_index}\t#{@hive-team}\t#{@hive-workspace}",
    ], check=False)
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t")
        while len(parts) < 3:
            parts.append("")
        if parts[1]:
            entry = by_name.setdefault(parts[1], {"name": parts[1], "workspace": ""})
            entry["tmuxWindow"] = parts[0]
            entry["tmuxSession"] = parts[0].split(":")[0] if ":" in parts[0] else ""
            entry["workspace"] = entry.get("workspace") or parts[2]
    for entry in by_name.values():
        entry.setdefault("tmuxWindow", "")
        entry.setdefault("tmuxSession", "")
    return list(by_name.values())
