"""Programmatic message delivery for Claude panes via Claude Code "channels".

Mirrors the codex app-server adapter's role for Claude: ``Agent.send`` hands a
``<HIVE>`` envelope to :func:`send_to_pane`, which writes it to a per-pane unix
socket. The channel MCP server (``claude_channel_server``, spawned by Claude
from a project ``.mcp.json``) turns that into a ``notifications/claude/channel``
push -- no tmux send-keys, no composer draft disturbance. Claude delivery is
channel-only; a ``False`` from :func:`send_to_pane` is a delivery failure that
surfaces through the sidecar's msgId-render tracking, not a keystroke fallback.

Channel registration: write the MCP server into the project ``.mcp.json`` at the
git root and keep it out of ``git status`` (repo-local ``info/exclude`` for an
untracked file; ``skip-worktree`` for a clean tracked file inside a worktree).
The server name resolves for the
``--dangerously-load-development-channels server:hive-channel`` launch flag.
"""
from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import sys
from pathlib import Path

SERVER_NAME = "hive-channel"
_MCP_FILENAME = ".mcp.json"
_EXCLUDE_PATTERN = "/.mcp.json"
_MSGID_RE = re.compile(r"msgId=([^\s>]+)")
_SOCKET_CONNECT_TIMEOUT = 2.0


# --- paths / readiness ------------------------------------------------------

def _hive_home() -> Path:
    """Read $HIVE_HOME fresh (matches context.HIVE_HOME's formula) so spawned
    panes and tests resolve the same short socket root the server uses."""
    return Path(os.environ.get("HIVE_HOME") or (Path.home() / ".hive"))


def _channel_dir() -> Path:
    return _hive_home() / "channel"


def _slug(pane: str) -> str:
    return pane.replace("%", "") or "default"


def channel_socket_path(pane: str) -> Path:
    """Per-pane socket under a short ``$HIVE_HOME`` path (sun_path limit safe)."""
    return _channel_dir() / f"hive-pane-{_slug(pane)}.sock"


def ready_marker_path(pane: str) -> Path:
    return _channel_dir() / f"hive-pane-{_slug(pane)}.ready"


def mark_ready(pane: str) -> None:
    """Record that the channel registered for this pane (set at spawn time)."""
    directory = _channel_dir()
    directory.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(directory, 0o700)
    except OSError:
        pass
    ready_marker_path(pane).write_text("1")


def clear_ready(pane: str) -> None:
    try:
        ready_marker_path(pane).unlink()
    except OSError:
        pass


def is_ready(pane: str) -> bool:
    return ready_marker_path(pane).exists()


# --- git / project resolution ----------------------------------------------

def _git(root: str, *args: str) -> subprocess.CompletedProcess | None:
    try:
        return subprocess.run(
            ["git", "-C", root, *args],
            capture_output=True, text=True, timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None


def project_root(cwd: str) -> str:
    """Claude discovers project ``.mcp.json`` at the git root, not an arbitrary
    cwd. Use the toplevel when in a repo; fall back to cwd otherwise."""
    out = _git(cwd, "rev-parse", "--show-toplevel")
    if out is not None and out.returncode == 0 and out.stdout.strip():
        return out.stdout.strip()
    return cwd


def _exclude_path(root: str) -> str | None:
    # NOTE: for a linked worktree this resolves to the *repo-local* common
    # info/exclude (<common-dir>/info/exclude), which git status honors;
    # a worktree-local exclude is not honored. So .mcp.json is hidden for
    # every worktree + main, which is fine -- it is always generated.
    out = _git(root, "rev-parse", "--git-path", "info/exclude")
    if out is None or out.returncode != 0 or not out.stdout.strip():
        return None
    p = out.stdout.strip()
    return p if os.path.isabs(p) else os.path.join(root, p)


def _is_tracked(root: str) -> bool:
    out = _git(root, "ls-files", "--error-unmatch", _MCP_FILENAME)
    return out is not None and out.returncode == 0


def _ensure_excluded(root: str) -> None:
    path = _exclude_path(root)
    if not path:
        return
    try:
        existing = Path(path).read_text() if os.path.exists(path) else ""
    except OSError:
        return
    if any(line.strip() == _EXCLUDE_PATTERN for line in existing.splitlines()):
        return
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "a") as fh:
            if existing and not existing.endswith("\n"):
                fh.write("\n")
            fh.write(_EXCLUDE_PATTERN + "\n")
    except OSError:
        pass


# --- spawn-time config ------------------------------------------------------

def _hive_import_root() -> str:
    """Directory to put on the child's PYTHONPATH so the spawned MCP server
    imports the *same* hive as this process (source lane vs installed)."""
    import hive
    return str(Path(hive.__file__).resolve().parent.parent)


def _child_pythonpath() -> str:
    root = _hive_import_root()
    current = os.environ.get("PYTHONPATH", "")
    return root + (os.pathsep + current if current else "")


def _server_entry() -> dict:
    return {
        "type": "stdio",  # canonical Claude Code project-MCP server shape
        "command": sys.executable,
        "args": ["-m", "hive.adapters.claude_channel_server"],
        "env": {"HIVE_HOME": str(_hive_home()), "PYTHONPATH": _child_pythonpath()},
    }


def _in_linked_worktree(root: str) -> bool:
    out = _git(root, "rev-parse", "--git-dir")
    return out is not None and out.returncode == 0 and "/worktrees/" in out.stdout


def _mcp_dirty(root: str) -> bool:
    """True if a tracked .mcp.json has uncommitted (working-tree or staged)
    changes. Anything but a clean exit 0 is treated as unsafe-to-touch."""
    out = _git(root, "diff", "HEAD", "--quiet", "--", _MCP_FILENAME)
    return out is None or out.returncode != 0


def _channel_unavailable(mcp_path: str, reason: str) -> list[str]:
    """Surface a hard setup failure (the validator requires this over silently
    hiding/losing work) and disable the channel for this pane."""
    sys.stderr.write(
        f"[hive-channel] channel not registered: {mcp_path} {reason}. "
        f"This claude pane will not receive hive messages.\n"
    )
    return []


def prepare_pane(cwd: str) -> list[str]:
    """Register the channel MCP server and return Claude launch flags.

    ``.mcp.json`` is a shared project MCP config (Claude Code and Codex both read
    it), so hive-channel is **merged** into an existing JSON object, preserving
    every other server and top-level key. A file that is not a JSON object is
    replaced. Hiding the local addition from git is only safe where it can be
    cleaned up, so a tracked ``.mcp.json`` is handled conservatively:

    - **untracked / absent** -> merge/create, hide via repo-local ``info/exclude``;
    - **tracked, clean, inside a hive worktree** -> merge + ``skip-worktree``;
      the bit lives in this worktree's index and dies with the worktree on
      ``hive worktree done`` (verified isolated from the main checkout);
    - **tracked with uncommitted changes**, or **tracked outside a worktree**
      (where skip-worktree could not be cleaned up) -> refuse, surface, and
      never touch the file. Channel-only, so such a pane is unsupported.
    """
    root = project_root(cwd)
    mcp_path = os.path.join(root, _MCP_FILENAME)
    tracked = os.path.exists(mcp_path) and _is_tracked(root)

    if tracked:
        if _mcp_dirty(root):
            return _channel_unavailable(
                mcp_path, "is tracked and has uncommitted changes (commit or "
                "stash them, then respawn)")
        if not _in_linked_worktree(root):
            return _channel_unavailable(
                mcp_path, "is tracked outside a hive worktree, where the "
                "registration could not be cleaned up (spawn in a worktree)")

    cfg: dict = {}
    if os.path.exists(mcp_path):
        try:
            loaded = json.loads(Path(mcp_path).read_text())
        except (OSError, ValueError):
            loaded = None
        if isinstance(loaded, dict):
            cfg = loaded  # merge: keep the user's / Codex's servers and keys

    servers = cfg.get("mcpServers")
    if not isinstance(servers, dict):
        servers = cfg["mcpServers"] = {}
    servers[SERVER_NAME] = _server_entry()
    try:
        Path(mcp_path).write_text(json.dumps(cfg, indent=2) + "\n")
    except OSError:
        return []
    if tracked:
        _git(root, "update-index", "--skip-worktree", _MCP_FILENAME)
    else:
        _ensure_excluded(root)
    return ["--dangerously-load-development-channels", f"server:{SERVER_NAME}"]


# --- delivery ---------------------------------------------------------------

def _extract_msg_id(text: str) -> str:
    m = _MSGID_RE.search(text)
    return m.group(1) if m else ""


def send_to_pane(pane: str, text: str) -> bool:
    """Deliver ``text`` over the pane's channel socket.

    Returns ``False`` (a delivery failure -- Claude is channel-only, so there is
    no keystroke fallback) when the channel is not locally ready: no ready marker
    (channel never registered), no socket, or a refused/timed-out connect. A
    successful write returns ``True``; the ready marker -- set only after Claude
    printed the channel registration notice -- is what distinguishes a live
    channel from a silently dropped one (channel notifications are not acked).
    """
    if not pane or not is_ready(pane):
        return False
    sock_path = channel_socket_path(pane)
    if not sock_path.exists():
        return False
    payload = json.dumps({"msg_id": _extract_msg_id(text), "content": text})
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(_SOCKET_CONNECT_TIMEOUT)
    try:
        conn.connect(str(sock_path))
        conn.sendall(payload.encode("utf-8"))
        conn.shutdown(socket.SHUT_WR)
        return True
    except OSError:
        return False
    finally:
        conn.close()
