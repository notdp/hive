"""Team: a tmux window with a group of agents."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import tmux
from .agent import Agent, detect_current_session_id
from .agent_cli import member_role_for_pane, resolve_session_id_for_pane

HIVE_HOME = Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))
COLORS = ["green", "blue", "yellow", "red", "magenta", "cyan"]
LEAD_AGENT_NAME = "orch"
_TMUX_REQUIRED_MESSAGE = "Hive requires tmux. Start or attach to a tmux session first."


def _session_id_for_pane(pane_id: str, current_session_id: str | None = None) -> str | None:
    if current_session_id:
        return current_session_id
    if not pane_id:
        return current_session_id
    return resolve_session_id_for_pane(pane_id) or current_session_id


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

    @property
    def teams_dir(self) -> Path:
        return HIVE_HOME / "teams" / self.name

    @property
    def config_path(self) -> Path:
        return self.teams_dir / "config.json"

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
        config_path = HIVE_HOME / "teams" / name / "config.json"
        if config_path.exists():
            raise ValueError(f"Team '{name}' already exists")
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        resolved_cwd = cwd or os.getcwd()
        team = cls(name=name, description=description, workspace=workspace)

        team.lead_pane_id = tmux.get_current_pane_id() or ""
        team.lead_session_id = detect_current_session_id(resolved_cwd, pane_id=team.lead_pane_id)
        team.tmux_session = tmux.get_current_session_name() or ""
        team.tmux_window = tmux.get_current_window_target() or ""
        if team.lead_pane_id:
            tmux.tag_pane(team.lead_pane_id, member_role_for_pane(team.lead_pane_id), team.lead_name, name)

        team.teams_dir.mkdir(parents=True, exist_ok=True)

        team.save()
        return team

    @classmethod
    def load(cls, name: str) -> Team:
        """Load an existing team from config."""
        config_path = HIVE_HOME / "teams" / name / "config.json"
        if not config_path.exists():
            raise FileNotFoundError(f"Team '{name}' not found")

        with open(config_path) as f:
            data = json.load(f)

        team = cls(
            name=data["name"],
            description=data.get("description", ""),
            workspace=data.get("workspace", ""),
            lead_name=data.get("leadName", LEAD_AGENT_NAME),
            created_at=data.get("createdAt", 0),
            lead_pane_id=data.get("leadPaneId", ""),
            lead_session_id=data.get("leadSessionId"),
            tmux_session=data.get("tmuxSession", ""),
            tmux_window=data.get("tmuxWindow", ""),
        )

        for member in data.get("members", []):
            agent = Agent(
                name=member["name"],
                team_name=name,
                pane_id=member.get("tmuxPaneId", ""),
                model=member.get("model", ""),
                prompt=member.get("prompt", ""),
                color=member.get("color", "green"),
                cwd=member.get("cwd", ""),
                session_id=member.get("sessionId"),
                spawned_at=member.get("spawnedAt", 0),
            )
            team.agents[agent.name] = agent

        for term in data.get("terminals", []):
            terminal = Terminal(name=term["name"], pane_id=term.get("tmuxPaneId", ""))
            team.terminals[terminal.name] = terminal

        return team

    def is_tmux_alive(self) -> bool:
        """Check if the tmux environment this team was created in still exists.

        Checks both session existence and lead pane liveness, because the same
        session may outlive the window/panes where the team was created.
        """
        if not self.tmux_session:
            return True  # Legacy teams without session binding are always "alive"
        if not tmux.has_session(self.tmux_session):
            return False
        if self.lead_pane_id and not tmux.is_pane_alive(self.lead_pane_id):
            return False
        return True

    def save(self) -> None:
        """Save team config to disk."""
        data = {
            "name": self.name,
            "description": self.description,
            "workspace": self.workspace,
            "leadName": self.lead_name,
            "leadPaneId": self.lead_pane_id,
            "leadSessionId": self.lead_session_id,
            "tmuxSession": self.tmux_session,
            "tmuxWindow": self.tmux_window,
            "createdAt": self.created_at,
            "members": [a.to_dict() for a in self.agents.values()],
            "terminals": [t.to_dict() for t in self.terminals.values()],
        }
        self.teams_dir.mkdir(parents=True, exist_ok=True)
        with open(self.config_path, "w") as f:
            json.dump(data, f, indent=2)

    def lead_agent(self) -> Agent | None:
        if not self.lead_pane_id:
            return None
        return Agent(
            name=self.lead_name,
            team_name=self.name,
            pane_id=self.lead_pane_id,
            cwd=os.getcwd(),
            session_id=self.lead_session_id,
        )

    # --- Agent management ---

    def spawn(
        self,
        name: str,
        model: str = "",
        prompt: str = "",
        color: str = "",
        cwd: str = "",
        skill: str = "hive",
        workflow: str = "",
        extra_env: dict[str, str] | None = None,
    ) -> Agent:
        """Spawn a new agent in the team."""
        if name in self.agents:
            raise ValueError(f"Agent '{name}' already exists in team '{self.name}'")
        if not tmux.is_inside_tmux():
            raise ValueError(_TMUX_REQUIRED_MESSAGE)

        if not color:
            idx = len(self.agents) % len(COLORS)
            color = COLORS[idx]

        is_first = len(self.agents) == 0
        if is_first:
            target = self.lead_pane_id or tmux.get_current_pane_id() or ""
            split_horizontal = True
            split_size = "50%"
        else:
            # Subsequent: split from last agent pane vertically
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
            color=color,
            cwd=cwd or os.getcwd(),
            is_first=is_first,
            split_horizontal=split_horizontal,
            split_size=split_size,
            skill=skill,
            extra_env=extra_env,
        )

        if workflow:
            agent.load_skill(workflow)

        tmux.tag_pane(agent.pane_id, "agent", name, self.name)
        self.agents[name] = agent

        # Layout and pane borders
        window_target = tmux.get_current_window_target()
        if window_target:
            tmux.enable_pane_border_status(window_target)
            tmux.set_window_option(window_target, "main-pane-width", "50%")
            tmux.select_layout(window_target, "main-vertical")

        self.save()
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
        changed = False
        lead = self.lead_agent()
        if lead is not None:
            refreshed_lead_session = _session_id_for_pane(lead.pane_id, lead.session_id)
            if refreshed_lead_session != lead.session_id:
                lead.session_id = refreshed_lead_session
                self.lead_session_id = refreshed_lead_session
                changed = True
            members.append({
                "name": lead.name,
                "role": member_role_for_pane(lead.pane_id),
                "alive": lead.is_alive(),
                "pane": lead.pane_id,
                "model": lead.model,
                "color": lead.color,
                "sessionId": refreshed_lead_session,
            })
        for name in sorted(self.agents):
            agent = self.agents[name]
            refreshed_session = _session_id_for_pane(agent.pane_id, agent.session_id)
            if refreshed_session != agent.session_id:
                agent.session_id = refreshed_session
                changed = True
            members.append({
                "name": name,
                "role": "agent",
                "alive": agent.is_alive(),
                "pane": agent.pane_id,
                "model": agent.model,
                "color": agent.color,
                "sessionId": refreshed_session,
            })
        for name in sorted(self.terminals):
            terminal = self.terminals[name]
            members.append({
                "name": name,
                "role": "terminal",
                "alive": terminal.is_alive(),
                "pane": terminal.pane_id,
            })
        if changed:
            self.save()
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
