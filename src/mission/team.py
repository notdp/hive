"""Team: a tmux session with a group of droid agents."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import tmux
from .agent import Agent

MISSION_HOME = Path(os.environ.get("MISSION_HOME", str(Path.home() / ".mission")))
COLORS = ["green", "blue", "yellow", "red", "magenta", "cyan"]


@dataclass
class Team:
    name: str
    description: str = ""
    lead_name: str = "team-lead"
    agents: dict[str, Agent] = field(default_factory=dict)
    created_at: float = field(default_factory=time.time)
    lead_pane_id: str = ""

    @property
    def teams_dir(self) -> Path:
        return MISSION_HOME / "teams" / self.name

    @property
    def config_path(self) -> Path:
        return self.teams_dir / "config.json"

    @property
    def inboxes_dir(self) -> Path:
        return self.teams_dir / "inboxes"

    @property
    def tasks_dir(self) -> Path:
        return MISSION_HOME / "tasks" / self.name

    # --- Lifecycle ---

    @classmethod
    def create(
        cls,
        name: str,
        description: str = "",
        cwd: str = "",
    ) -> Team:
        """Create a new team.

        If inside tmux: use current window (split panes in-place).
        If outside tmux: create a new detached session.
        """
        team = cls(name=name, description=description)

        if tmux.is_inside_tmux():
            team.lead_pane_id = tmux.get_current_pane_id() or ""
        else:
            if tmux.has_session(name):
                raise ValueError(f"Team '{name}' already exists")
            tmux.new_session(name)

        # Create directories
        team.teams_dir.mkdir(parents=True, exist_ok=True)
        team.inboxes_dir.mkdir(parents=True, exist_ok=True)
        team.tasks_dir.mkdir(parents=True, exist_ok=True)

        team.save()
        return team

    @classmethod
    def load(cls, name: str) -> Team:
        """Load an existing team from config."""
        config_path = MISSION_HOME / "teams" / name / "config.json"
        if not config_path.exists():
            raise FileNotFoundError(f"Team '{name}' not found")

        with open(config_path) as f:
            data = json.load(f)

        team = cls(
            name=data["name"],
            description=data.get("description", ""),
            lead_name=data.get("leadName", "team-lead"),
            created_at=data.get("createdAt", 0),
            lead_pane_id=data.get("leadPaneId", ""),
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

        return team

    def save(self) -> None:
        """Save team config to disk."""
        data = {
            "name": self.name,
            "description": self.description,
            "leadName": self.lead_name,
            "leadPaneId": self.lead_pane_id,
            "createdAt": self.created_at,
            "members": [a.to_dict() for a in self.agents.values()],
        }
        self.teams_dir.mkdir(parents=True, exist_ok=True)
        with open(self.config_path, "w") as f:
            json.dump(data, f, indent=2)

    # --- Agent management ---

    def spawn(
        self,
        name: str,
        model: str = "",
        prompt: str = "",
        color: str = "",
        cwd: str = "",
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
                # First agent: split from lead pane, 70% for agent
                target = self.lead_pane_id or tmux.get_current_pane_id() or ""
                split_horizontal = True
                split_size = "70%"
            else:
                # Subsequent: split from last agent pane vertically
                last_agent = list(self.agents.values())[-1]
                target = last_agent.pane_id
                split_horizontal = False
                split_size = None
        else:
            panes = tmux.list_panes(self.name)
            target = panes[0] if panes else f"{self.name}:0"
            split_horizontal = True
            split_size = None

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
        )

        self.agents[name] = agent

        # Create inbox
        inbox_file = self.inboxes_dir / f"{name}.json"
        if not inbox_file.exists():
            inbox_file.write_text("[]")

        # Rebalance: main-vertical with lead at 30%
        if in_tmux:
            window_target = tmux.get_current_window_target()
            if window_target:
                tmux.select_layout(window_target, "main-vertical")
                lead = self.lead_pane_id or tmux.get_current_pane_id() or ""
                if lead:
                    tmux.resize_pane(lead, width="30%")
        elif len(self.agents) > 1:
            tmux.select_layout(self.name, "tiled")

        self.save()
        return agent

    def get(self, name: str) -> Agent:
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
        return {
            "name": self.name,
            "description": self.description,
            "agents": {
                name: {
                    "alive": agent.is_alive(),
                    "pane": agent.pane_id,
                    "model": agent.model,
                    "color": agent.color,
                }
                for name, agent in self.agents.items()
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
        # Only kill session if it was created by mission (not the user's session)
        if not tmux.is_inside_tmux():
            tmux.kill_session(self.name)
