"""Team: a tmux window with a group of droid agents."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import tmux
from .agent import Agent, detect_current_session_id

HIVE_HOME = Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))
COLORS = ["green", "blue", "yellow", "red", "magenta", "cyan"]


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
    lead_name: str = "orchestrator"
    agents: dict[str, Agent] = field(default_factory=dict)
    terminals: dict[str, Terminal] = field(default_factory=dict)
    created_at: float = field(default_factory=time.time)
    lead_pane_id: str = ""
    lead_session_id: str | None = None
    tmux_session: str = ""

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
        """Create a new team.

        If inside tmux: use current window (split panes in-place).
        If outside tmux: create a new detached session.
        """
        config_path = HIVE_HOME / "teams" / name / "config.json"
        if config_path.exists():
            raise ValueError(f"Team '{name}' already exists")

        resolved_cwd = cwd or os.getcwd()
        team = cls(name=name, description=description, workspace=workspace)

        if tmux.is_inside_tmux():
            team.lead_pane_id = tmux.get_current_pane_id() or ""
            team.lead_session_id = detect_current_session_id(resolved_cwd)
            if team.lead_pane_id:
                tmux.tag_pane(team.lead_pane_id, "lead", "orchestrator", name)
        else:
            if tmux.has_session(name):
                raise ValueError(f"Team '{name}' already exists")
            tmux.new_session(name)

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
            lead_name=data.get("leadName", "orchestrator"),
            created_at=data.get("createdAt", 0),
            lead_pane_id=data.get("leadPaneId", ""),
            lead_session_id=data.get("leadSessionId"),
            tmux_session=data.get("tmuxSession", ""),
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

        if not color:
            idx = len(self.agents) % len(COLORS)
            color = COLORS[idx]

        is_first = len(self.agents) == 0
        in_tmux = tmux.is_inside_tmux()

        if in_tmux:
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
        else:
            panes = tmux.list_panes(self.name)
            target = panes[0] if panes else f"{self.name}:0"
            split_horizontal = True
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
        if in_tmux:
            window_target = tmux.get_current_window_target()
            if window_target:
                tmux.enable_pane_border_status(window_target)
                tmux.set_window_option(window_target, "main-pane-width", "50%")
                tmux.select_layout(window_target, "main-vertical")
        elif len(self.agents) > 1:
            tmux.select_layout(self.name, "tiled")

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
        agents: dict[str, dict[str, object]] = {}
        lead = self.lead_agent()
        if lead is not None:
            agents[lead.name] = {
                "alive": lead.is_alive(),
                "pane": lead.pane_id,
                "model": lead.model,
                "color": lead.color,
                "sessionId": lead.session_id,
            }
        return {
            "name": self.name,
            "description": self.description,
            "workspace": self.workspace,
            "agents": {
                **agents,
                **{
                    name: {
                        "alive": agent.is_alive(),
                        "pane": agent.pane_id,
                        "model": agent.model,
                        "color": agent.color,
                        "sessionId": agent.session_id,
                    }
                    for name, agent in self.agents.items()
                },
            },
            "terminals": {
                name: {
                    "alive": terminal.is_alive(),
                    "pane": terminal.pane_id,
                }
                for name, terminal in self.terminals.items()
            },
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
        if not tmux.is_inside_tmux():
            tmux.kill_session(self.name)
