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
SETTINGS_FILE = Path.home() / ".factory" / "settings.json"
SESSIONS_DIR = Path.home() / ".factory" / "sessions"


def _shell_escape(s: str) -> str:
    """Escape a string for safe shell use."""
    return "'" + s.replace("'", "'\\''") + "'"


def _set_model_in_settings(model: str) -> str | None:
    """Set model in settings.json, return previous model value for restore."""
    if not model or not SETTINGS_FILE.is_file():
        return None
    with open(SETTINGS_FILE) as f:
        settings = json.load(f)
    base = model.replace("custom:", "", 1)
    target_id = model
    for m in settings.get("customModels", []):
        if m.get("model", "") == base or m.get("displayName", "") == base:
            target_id = m["id"]
            break
    defaults = settings.setdefault("sessionDefaultSettings", {})
    prev_model = defaults.get("model")
    if defaults.get("model") != target_id:
        defaults["model"] = target_id
        with open(SETTINGS_FILE, "w") as f:
            json.dump(settings, f, indent=2, ensure_ascii=False)
    return prev_model


def _encode_cwd(cwd: str) -> str:
    """Encode a CWD path to the factory sessions directory name format."""
    return "-" + cwd.lstrip("/").replace("/", "-")


def _list_sessions(cwd: str) -> set[str]:
    """List existing session UUIDs for a given CWD."""
    sessions_path = SESSIONS_DIR / _encode_cwd(cwd)
    if not sessions_path.is_dir():
        return set()
    return {
        f.name.removesuffix(".settings.json")
        for f in sessions_path.iterdir()
        if f.name.endswith(".settings.json")
    }


def _read_session_model(cwd: str, session_id: str) -> str | None:
    """Read the model from a session's settings.json."""
    path = SESSIONS_DIR / _encode_cwd(cwd) / f"{session_id}.settings.json"
    try:
        with open(path) as f:
            return json.load(f).get("model")
    except (OSError, json.JSONDecodeError):
        return None


def _detect_new_session(cwd: str, before: set[str], model: str = "") -> str | None:
    """Find a session UUID that appeared after spawn."""
    after = _list_sessions(cwd)
    new = after - before
    if not new:
        return None
    if len(new) == 1:
        return new.pop()
    if model:
        for sid in new:
            m = _read_session_model(cwd, sid)
            if m and model in m:
                return sid
    return new.pop()


def _restore_model_in_settings(prev_model: str | None) -> None:
    """Restore previous model in settings.json."""
    if prev_model is None or not SETTINGS_FILE.is_file():
        return
    with open(SETTINGS_FILE) as f:
        settings = json.load(f)
    defaults = settings.setdefault("sessionDefaultSettings", {})
    if defaults.get("model") != prev_model:
        defaults["model"] = prev_model
        with open(SETTINGS_FILE, "w") as f:
            json.dump(settings, f, indent=2, ensure_ascii=False)


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
        skill: str = "mission",
        extra_env: dict[str, str] | None = None,
    ) -> Agent:
        """Spawn a droid in a tmux pane."""
        cwd = cwd or os.getcwd()

        # Snapshot existing sessions to detect the new one after startup
        sessions_before = _list_sessions(cwd)

        # Set model in settings.json before starting droid
        prev_model = _set_model_in_settings(model)

        if is_first and not tmux.is_inside_tmux():
            pane_id = target_pane
        else:
            pane_id = tmux.split_window(target_pane, horizontal=split_horizontal, size=split_size)

        tmux.set_pane_title(pane_id, f"[{name}]")
        tmux.set_pane_border_color(pane_id, color)

        cmd_parts = ["exec", DROID_BIN]
        if session_id:
            cmd_parts.extend(["-r", session_id])

        env_parts = [
            f"MISSION_TEAM_NAME={_shell_escape(team_name)}",
            f"MISSION_AGENT_NAME={_shell_escape(name)}",
        ]
        if extra_env:
            for k, v in extra_env.items():
                env_parts.append(f"{k}={_shell_escape(v)}")
        env_vars = " ".join(env_parts)

        cmd = f"cd {_shell_escape(cwd)} && export {env_vars} && {' '.join(cmd_parts)}"
        tmux.send_keys(pane_id, cmd)

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

        if tmux.wait_for_text(pane_id, "for help", timeout=DROID_STARTUP_TIMEOUT):
            # Droid is ready — detect session ID and restore model
            detected_session = _detect_new_session(cwd, sessions_before, model=model)
            if detected_session:
                agent.session_id = detected_session

            _restore_model_in_settings(prev_model)
            time.sleep(1)

            if skill and skill != "none":
                tmux.send_keys(pane_id, f"/skill {skill}")
                time.sleep(2)

            # Only send mission bootstrap if using mission skill with a prompt
            if skill == "mission" and prompt:
                tmux.send_keys(pane_id,
                    "I am a mission teammate. "
                    "Run `mission read` to get my task, then execute it."
                )
        else:
            # Droid didn't start in time — still restore settings
            _restore_model_in_settings(prev_model)

        return agent

    # --- Control ---

    def send(self, text: str) -> None:
        """Send a prompt to the droid TUI."""
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
