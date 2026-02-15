"""Agent: a droid instance running in a tmux pane."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import tmux

DROID_BIN = os.environ.get("DROID_PATH", str(Path.home() / ".local" / "bin" / "droid"))
DROID_STARTUP_TIMEOUT = 30


@dataclass
class Agent:
    name: str
    team_name: str
    pane_id: str
    model: str = ""
    prompt: str = ""
    color: str = "green"
    cwd: str = field(default_factory=os.getcwd)
    session_id: str | None = None
    spawned_at: float = field(default_factory=time.time)

    # --- Lifecycle ---

    @classmethod
    def spawn(
        cls,
        name: str,
        team_name: str,
        target_pane: str,
        model: str = "",
        prompt: str = "",
        color: str = "green",
        cwd: str = "",
        session_id: str | None = None,
        is_first: bool = False,
        split_horizontal: bool = True,
        split_size: str | None = None,
    ) -> Agent:
        """Spawn a droid in a tmux pane."""
        cwd = cwd or os.getcwd()

        if is_first and not tmux.is_inside_tmux():
            pane_id = target_pane
        else:
            pane_id = tmux.split_window(target_pane, horizontal=split_horizontal, size=split_size)

        tmux.set_pane_title(pane_id, f"[{name}]")
        tmux.set_pane_border_color(pane_id, color)

        # Build droid command
        cmd_parts = [DROID_BIN]
        if session_id:
            cmd_parts.extend(["-r", session_id])
        if model:
            # Set via environment or TUI shortcut after startup
            pass

        # Set environment variables
        env_prefix = (
            f"export MISSION_TEAM_NAME={team_name} "
            f"MISSION_AGENT_NAME={name} && "
        )

        cmd = env_prefix + " ".join(cmd_parts)
        tmux.send_keys(pane_id, f"cd {cwd} && {cmd}")

        agent = cls(
            name=name,
            team_name=team_name,
            pane_id=pane_id,
            model=model,
            prompt=prompt,
            color=color,
            cwd=cwd,
            session_id=session_id,
        )

        # Wait for droid TUI to start
        if tmux.wait_for_text(pane_id, "for help", timeout=DROID_STARTUP_TIMEOUT):
            # Send initial prompt if provided
            if prompt:
                time.sleep(1)
                agent.send(prompt)

        return agent

    # --- Control ---

    def send(self, text: str) -> None:
        """Send a message to the droid TUI."""
        tmux.send_keys(self.pane_id, text)

    def interrupt(self) -> None:
        """Press Escape to interrupt."""
        tmux.send_key(self.pane_id, "Escape")

    def capture(self, lines: int = 50) -> str:
        """Capture pane output."""
        return tmux.capture_pane(self.pane_id, lines)

    def is_alive(self) -> bool:
        return tmux.is_pane_alive(self.pane_id)

    def shutdown(self) -> None:
        """Send Ctrl+C twice then exit."""
        tmux.send_key(self.pane_id, "C-c")
        time.sleep(0.5)
        tmux.send_key(self.pane_id, "C-c")
        time.sleep(0.5)
        tmux.send_keys(self.pane_id, "exit")

    def kill(self) -> None:
        """Force kill the pane."""
        tmux.kill_pane(self.pane_id)

    # --- TUI shortcuts ---

    def switch_model(self) -> None:
        """Ctrl+N to cycle models."""
        tmux.send_key(self.pane_id, "C-n")

    def switch_autonomy(self) -> None:
        """Ctrl+L to cycle autonomy levels."""
        tmux.send_key(self.pane_id, "C-l")

    def toggle_mode(self) -> None:
        """Shift+Tab to toggle auto/spec mode."""
        tmux.send_key(self.pane_id, "BTab")

    # --- Serialization ---

    def to_dict(self) -> dict:
        return {
            "agentId": f"{self.name}@{self.team_name}",
            "name": self.name,
            "model": self.model,
            "prompt": self.prompt,
            "color": self.color,
            "cwd": self.cwd,
            "tmuxPaneId": self.pane_id,
            "sessionId": self.session_id,
            "spawnedAt": self.spawned_at,
            "isActive": self.is_alive(),
        }
