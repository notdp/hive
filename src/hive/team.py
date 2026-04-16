"""Team: a tmux window with a group of agents."""

from __future__ import annotations

import os
import time
from dataclasses import dataclass, field

from . import tmux
from .agent import Agent
from .agent_cli import member_role_for_pane

HIVE_HOME = __import__("pathlib").Path(os.environ.get("HIVE_HOME", str(__import__("pathlib").Path.home() / ".hive")))
LEAD_AGENT_NAME = "orch"
_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."

@dataclass
class Terminal:
    name: str
    pane_id: str

    def is_alive(self) -> bool:
        return tmux.is_pane_alive(self.pane_id)

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "tmuxPaneId": self.pane_id,
            "isActive": self.is_alive(),
        }


@dataclass
class Team:
    name: str
    description: str = ""
    workspace: str = ""
    lead_name: str = LEAD_AGENT_NAME
    agents: dict[str, Agent] = field(default_factory=dict)
    terminals: dict[str, Terminal] = field(default_factory=dict)
    created_at: float = field(default_factory=time.time)
    lead_pane_id: str = ""
    lead_session_id: str | None = None
    tmux_session: str = ""
    tmux_window: str = ""
    tmux_window_id: str = ""

    # --- Window-level tmux options ---

    def _write_window_options(self) -> None:
        target = self.tmux_window
        if not target:
            return
        tmux.enable_pane_border_status(target)
        tmux.set_window_option(target, "@hive-team", self.name)
        tmux.set_window_option(target, "@hive-workspace", self.workspace)
        if self.description:
            tmux.set_window_option(target, "@hive-desc", self.description)
        tmux.set_window_option(target, "@hive-created", str(self.created_at))

    # --- Lifecycle ---

    @classmethod
    def create(
        cls,
        name: str,
        description: str = "",
        cwd: str = "",
        workspace: str = "",
    ) -> Team:
        """Create a new team in the current tmux window."""
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        window_target = tmux.get_current_window_target() or ""
        existing_team = tmux.get_window_option(window_target, "hive-team") if window_target else None
        if existing_team:
            raise ValueError(f"Team '{existing_team}' already exists in this window")

        resolved_cwd = cwd or os.getcwd()
        team = cls(name=name, description=description, workspace=workspace)

        team.lead_pane_id = tmux.get_current_pane_id() or ""
        from .agent import detect_current_session_id
        team.lead_session_id = detect_current_session_id(resolved_cwd, pane_id=team.lead_pane_id)
        team.tmux_session = tmux.get_current_session_name() or ""
        team.tmux_window = window_target
        team.tmux_window_id = tmux.get_current_window_id() or ""
        if team.lead_pane_id:
            tmux.tag_pane(team.lead_pane_id, member_role_for_pane(team.lead_pane_id), team.lead_name, name)

        team._write_window_options()
        return team

    @classmethod
    def load(cls, name: str, *, prefer_pane: str = "") -> Team:
        """Load a team by scanning tmux window options and pane tags.

        Searches all windows in all sessions for a window with @hive-team == name.
        When *prefer_pane* is given, its window is preferred when multiple
        windows claim the same team name.
        """
        hint = prefer_pane or tmux.get_current_pane_id() or ""
        window_target, window_data = _find_team_window(name, prefer_pane=hint)
        if not window_target:
            raise FileNotFoundError(f"Team '{name}' not found")

        team = cls(
            name=name,
            description=window_data.get("desc", ""),
            workspace=window_data.get("workspace", ""),
            created_at=float(window_data.get("created") or 0),
            tmux_session=window_target.split(":")[0] if ":" in window_target else "",
            tmux_window=window_target,
            tmux_window_id=window_data.get("window_id", ""),
        )

        panes = tmux.list_panes_full(window_target)
        for pane in panes:
            if pane.team != name:
                continue
            if pane.role in ("lead", "orchestrator", "agent", "terminal"):
                if pane.role in ("lead", "orchestrator"):
                    team.lead_pane_id = pane.pane_id
                    team.lead_name = pane.agent or LEAD_AGENT_NAME
                elif pane.role == "agent":
                    from .agent_cli import AGENT_CLI_NAMES, detect_profile_for_pane, normalize_command
                    resolved_cli = pane.cli or normalize_command(pane.command)
                    if resolved_cli not in AGENT_CLI_NAMES:
                        profile = detect_profile_for_pane(pane.pane_id)
                        resolved_cli = profile.name if profile else "droid"
                    agent = Agent(
                        name=pane.agent,
                        team_name=name,
                        pane_id=pane.pane_id,
                        cli=resolved_cli,
                        cwd=tmux.display_value(pane.pane_id, "#{pane_current_path}") or "",
                    )
                    team.agents[pane.agent] = agent
                elif pane.role == "terminal":
                    terminal = Terminal(name=pane.agent, pane_id=pane.pane_id)
                    team.terminals[pane.agent] = terminal

        return team

    def is_tmux_alive(self) -> bool:
        if not self.tmux_session:
            return True
        if not tmux.has_session(self.tmux_session):
            return False
        if self.lead_pane_id and not tmux.is_pane_alive(self.lead_pane_id):
            return False
        return True

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
        workflow: str = "",
        extra_env: dict[str, str] | None = None,
        cli: str = "droid",
    ) -> Agent:
        """Spawn a new agent in the team."""
        if name in self.agents:
            raise ValueError(f"Agent '{name}' already exists in team '{self.name}'")
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        is_first = len(self.agents) == 0
        if is_first:
            target = self.lead_pane_id or tmux.get_current_pane_id() or ""
            split_horizontal = True
            split_size = "50%"
        else:
            last_agent = list(self.agents.values())[-1]
            target = last_agent.pane_id
            split_horizontal = False
            split_size = "50%"

        initial_skill = workflow or skill

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
            skill=initial_skill,
            extra_env=extra_env,
            cli=cli,
            send_bootstrap_prompt=initial_skill == "hive",
        )

        tmux.tag_pane(agent.pane_id, "agent", name, self.name, cli=cli)
        self.agents[name] = agent

        window_target = tmux.get_current_window_target()
        if window_target:
            tmux.enable_pane_border_status(window_target)
            tmux.set_window_option(window_target, "main-pane-width", "50%")
            tmux.select_layout(window_target, "main-vertical")

        return agent

    def get(self, name: str) -> Agent:
        lead = self.lead_agent()
        if lead is not None and name == lead.name:
            return lead
        if name not in self.agents:
            raise KeyError(f"Agent '{name}' not found")
        return self.agents[name]

    def broadcast(self, text: str, exclude: str | None = None) -> None:
        """Send text to all agents."""
        for name, agent in self.agents.items():
            if name != exclude and agent.is_alive():
                agent.send(text)

    def status(self) -> dict:
        """Get team status."""
        members: list[dict[str, object]] = []
        lead = self.lead_agent()
        if lead is not None:
            members.append({
                "name": lead.name,
                "role": member_role_for_pane(lead.pane_id),
                "pane": lead.pane_id,
            })
        for name in sorted(self.agents):
            members.append({
                "name": name,
                "role": "agent",
                "pane": self.agents[name].pane_id,
            })
        for name in sorted(self.terminals):
            terminal = self.terminals[name]
            members.append({
                "name": name,
                "role": "terminal",
                "pane": terminal.pane_id,
            })
        return {
            "name": self.name,
            "description": self.description,
            "workspace": self.workspace,
            "tmuxSession": self.tmux_session,
            "tmuxWindow": self.tmux_window,
            "members": members,
        }

    def shutdown(self, name: str | None = None) -> None:
        """Shutdown one or all agents."""
        targets = [self.agents[name]] if name else list(self.agents.values())
        for agent in targets:
            agent.shutdown()

    def cleanup(self) -> None:
        """Kill all agent panes (not the session itself if in-place)."""
        for agent in self.agents.values():
            agent.kill()
        for terminal in self.terminals.values():
            if tmux.is_pane_alive(terminal.pane_id):
                tmux.clear_pane_tags(terminal.pane_id)
        if self.lead_pane_id and tmux.is_pane_alive(self.lead_pane_id):
            tmux.clear_pane_tags(self.lead_pane_id)


def _find_team_window(name: str, *, prefer_pane: str = "") -> tuple[str, dict[str, str]]:
    """Find the tmux window that hosts team *name* by scanning window options.

    When multiple windows claim the same team name (e.g. after a window
    move/reorder leaves stale tags), the window containing *prefer_pane*
    wins.  If *prefer_pane* is not supplied we fall back to the window
    that actually has panes tagged for the team.  Stale duplicates get
    their ``@hive-team`` tag stripped automatically.
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
        panes = tmux.list_panes_full(wt)
        if any(p.team == name and p.role for p in panes):
            _gc_stale_team_windows(name, keep=wt, all_windows=[c[0] for c in candidates])
            return wt, data

    # 3) Fall back to first match (shouldn't normally happen).
    return candidates[0]


def _gc_stale_team_windows(name: str, *, keep: str, all_windows: list[str]) -> None:
    """Remove @hive-team from windows that are stale duplicates."""
    for wt in all_windows:
        if wt == keep:
            continue
        for key in ("hive-team", "hive-workspace", "hive-desc", "hive-created"):
            tmux.clear_window_option(wt, f"@{key}")


def list_teams() -> list[dict[str, str]]:
    """List all teams by scanning tmux window options."""
    r = tmux._run([
        "list-windows", "-a", "-F",
        "#{session_name}:#{window_index}\t#{@hive-team}\t#{@hive-workspace}",
    ], check=False)
    teams = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t")
        while len(parts) < 3:
            parts.append("")
        if parts[1]:
            teams.append({
                "name": parts[1],
                "tmuxWindow": parts[0],
                "tmuxSession": parts[0].split(":")[0] if ":" in parts[0] else "",
                "workspace": parts[2],
            })
    return teams
