"""tmux operations: pane lifecycle, send_keys, capture_pane, layout."""

from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass


def _run(args: list[str], check: bool = True, timeout: int = 5) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(
            ["tmux", *args],
            capture_output=True, text=True, check=check, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(["tmux", *args], 1, "", "timeout")


def _run_output(args: list[str]) -> str:
    r = _run(args)
    return r.stdout.strip()


# --- Session ---

def has_session(name: str) -> bool:
    r = _run(["has-session", "-t", name], check=False)
    return r.returncode == 0


def new_session(name: str, width: int = 200, height: int = 50) -> str:
    """Create a detached tmux session. Returns the initial pane id."""
    r = _run([
        "new-session", "-d", "-s", name,
        "-x", str(width), "-y", str(height),
        "-P", "-F", "#{pane_id}",
    ])
    return r.stdout.strip()


def kill_session(name: str) -> None:
    _run(["kill-session", "-t", name], check=False)


# --- Pane ---

def split_window(
    target: str,
    horizontal: bool = True,
    size: str | None = None,
    detach: bool = True,
) -> str:
    """Split a window/pane. Returns the new pane id.

    detach=True (default) keeps focus on the original pane (-d flag).
    """
    args = ["split-window", "-t", target]
    if detach:
        args.append("-d")
    args.append("-h" if horizontal else "-v")
    if size:
        args.extend(["-l", size])
    args.extend(["-P", "-F", "#{pane_id}"])
    r = _run(args)
    return r.stdout.strip()


def send_keys(pane_id: str, text: str, enter: bool = True) -> None:
    """Send literal text to a pane, then optionally press Enter.

    Uses two separate tmux invocations to avoid command-chaining (;)
    interfering with literal text parsing, which caused truncation.
    """
    _run(["send-keys", "-t", pane_id, "-l", text])
    if enter:
        _run(["send-keys", "-t", pane_id, "Enter"])


def send_key(pane_id: str, key: str) -> None:
    """Send a special key (Escape, C-c, C-n, etc.)."""
    _run(["send-keys", "-t", pane_id, key])


def capture_pane(pane_id: str, lines: int = 50) -> str:
    """Capture pane content."""
    return _run_output([
        "capture-pane", "-t", pane_id, "-p", f"-S", f"-{lines}",
    ])


def is_pane_alive(pane_id: str) -> bool:
    r = _run(
        ["list-panes", "-a", "-F", "#{pane_id} #{pane_dead}"],
        check=False,
    )
    for line in r.stdout.strip().split("\n"):
        parts = line.split()
        if len(parts) >= 2 and parts[0] == pane_id:
            return parts[1] == "0"
    return False


def kill_pane(pane_id: str) -> None:
    _run(["kill-pane", "-t", pane_id], check=False)


# --- Layout & Appearance ---

def select_layout(target: str, layout: str = "tiled") -> None:
    _run(["select-layout", "-t", target, layout], check=False)


def set_pane_border_color(pane_id: str, color: str) -> None:
    _run([
        "select-pane", "-t", pane_id,
        "-P", f"fg={color}",
    ], check=False)


def set_pane_title(pane_id: str, title: str) -> None:
    _run([
        "select-pane", "-t", pane_id,
        "-T", title,
    ], check=False)


def enable_pane_border_status(target: str) -> None:
    """Enable pane border labels (shows pane titles at top of each pane)."""
    _run([
        "set-window-option", "-t", target,
        "pane-border-status", "top",
    ], check=False)
    _run([
        "set-window-option", "-t", target,
        "pane-border-format",
        " #{pane_title} ",
    ], check=False)


def set_window_option(target: str, option: str, value: str) -> None:
    _run(["set-window-option", "-t", target, option, value], check=False)


def resize_pane(pane_id: str, width: str | None = None, height: str | None = None) -> None:
    args = ["resize-pane", "-t", pane_id]
    if width:
        args.extend(["-x", width])
    if height:
        args.extend(["-y", height])
    _run(args, check=False)


def list_panes(target: str) -> list[str]:
    """List all pane ids in a window/session."""
    r = _run(["list-panes", "-t", target, "-F", "#{pane_id}"], check=False)
    return [p for p in r.stdout.strip().split("\n") if p]


# --- Context detection ---

def is_inside_tmux() -> bool:
    return bool(os.environ.get("TMUX"))


def get_current_pane_id() -> str | None:
    """Get the pane id of the calling process (per-pane env var)."""
    return os.environ.get("TMUX_PANE")


def get_current_window_target() -> str | None:
    """Get the window target that contains the calling pane."""
    pane_id = get_current_pane_id()
    if not pane_id:
        return None
    r = _run(
        ["display-message", "-t", pane_id, "-p", "#{session_name}:#{window_index}"],
        check=False,
    )
    return r.stdout.strip() or None


def get_current_session_name() -> str | None:
    """Get the tmux session name for the calling pane."""
    pane_id = get_current_pane_id()
    if not pane_id:
        return None
    r = _run(
        ["display-message", "-t", pane_id, "-p", "#{session_name}"],
        check=False,
    )
    return r.stdout.strip() or None


def get_current_window_index() -> str | None:
    """Get the window index for the calling pane."""
    pane_id = get_current_pane_id()
    if not pane_id:
        return None
    r = _run(
        ["display-message", "-t", pane_id, "-p", "#{window_index}"],
        check=False,
    )
    return r.stdout.strip() or None


@dataclass
class PaneInfo:
    pane_id: str
    title: str
    command: str = ""
    role: str = ""
    agent: str = ""
    team: str = ""


def list_panes_with_titles(target: str) -> list[PaneInfo]:
    """List all panes in a window with their IDs and titles."""
    r = _run(
        ["list-panes", "-t", target, "-F", "#{pane_id}\t#{pane_title}"],
        check=False,
    )
    result = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t", 1)
        pane_id = parts[0]
        title = parts[1] if len(parts) > 1 else ""
        result.append(PaneInfo(pane_id=pane_id, title=title))
    return result


_HIVE_FMT = "\t".join([
    "#{pane_id}", "#{pane_title}", "#{pane_current_command}",
    "#{@hive-role}", "#{@hive-agent}", "#{@hive-team}",
])


def list_panes_full(target: str) -> list[PaneInfo]:
    """List all panes with command and hive identity (@hive-role/agent/team)."""
    r = _run(["list-panes", "-t", target, "-F", _HIVE_FMT], check=False)
    result = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t")
        while len(parts) < 6:
            parts.append("")
        result.append(PaneInfo(
            pane_id=parts[0], title=parts[1], command=parts[2],
            role=parts[3], agent=parts[4], team=parts[5],
        ))
    return result


# --- Per-pane user options (@hive-*) ---

def set_pane_option(pane_id: str, key: str, value: str) -> None:
    _run(["set-option", "-p", "-t", pane_id, f"@{key}", value], check=False)


def get_pane_option(pane_id: str, key: str) -> str | None:
    r = _run(["display-message", "-t", pane_id, "-p", f"#{{@{key}}}"], check=False)
    val = r.stdout.strip()
    return val or None


def clear_pane_option(pane_id: str, key: str) -> None:
    _run(["set-option", "-p", "-t", pane_id, "-u", f"@{key}"], check=False)


def tag_pane(pane_id: str, role: str, agent: str, team: str) -> None:
    """Set all hive identity options on a pane."""
    set_pane_option(pane_id, "hive-role", role)
    set_pane_option(pane_id, "hive-agent", agent)
    set_pane_option(pane_id, "hive-team", team)


def clear_pane_tags(pane_id: str) -> None:
    """Remove all hive identity options from a pane."""
    for key in ("hive-role", "hive-agent", "hive-team"):
        clear_pane_option(pane_id, key)


# --- Utility ---

def wait_for_text(
    pane_id: str,
    text: str,
    timeout: float = 30,
    interval: float = 1,
) -> bool:
    """Wait until text appears in pane output."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        output = capture_pane(pane_id)
        if text in output:
            return True
        time.sleep(interval)
    return False
