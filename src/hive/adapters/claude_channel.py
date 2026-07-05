"""Programmatic message delivery for Claude panes via Claude Code "channels".

Mirrors the codex app-server adapter's role for Claude: ``Agent.send`` hands a
``<HIVE>`` envelope to :func:`send_to_pane`, which writes it to a per-pane unix
socket. The channel MCP server (``claude_channel_server``, spawned by Claude
as a stdio MCP child) turns that into a ``notifications/claude/channel``
push -- no tmux send-keys, no composer draft disturbance. Claude delivery is
channel-only; on a ``False`` from :func:`send_to_pane`, ``Agent.send`` raises
:class:`ChannelDeliveryError`, which callers surface as an explicit submit
failure (the sidecar projects it to ``injectStatus=failed``).

Channel registration: a static MCP config under ``$HIVE_HOME/channel`` passed
via ``--mcp-config`` -- no project file is ever touched. (An earlier project
``.mcp.json`` merge + git-hiding mechanism existed only because
``--mcp-config`` did not bring up channels on the Claude Code available at
the time: 2.1.187 was known broken, 2.1.198 is verified working, the exact
minimum is unverified.) Both ``--dangerously-load-development-channels`` and
``--mcp-config`` are variadic, so the caller must terminate the flag list
(spawn puts ``--`` before the positional prompt) or append these flags after
all positionals.
"""
from __future__ import annotations

import json
import os
import re
import socket
import sys
from pathlib import Path

SERVER_NAME = "hive-channel"
_MSGID_RE = re.compile(r"msgId=([^\s>]+)")
_SOCKET_CONNECT_TIMEOUT = 2.0


class ChannelDeliveryError(RuntimeError):
    """claude delivery is channel-only: raised when a pane has no usable
    channel (never registered, marker missing, or dead socket)."""


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
    """Written by the channel server once its socket listens; cleared by the
    server on exit and by spawn before a fresh launch (stale-marker guard)."""
    return _channel_dir() / f"hive-pane-{_slug(pane)}.ready"


def clear_ready(pane: str) -> None:
    try:
        ready_marker_path(pane).unlink()
    except OSError:
        pass


def is_ready(pane: str) -> bool:
    return ready_marker_path(pane).exists()


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
        "type": "stdio",  # canonical Claude Code MCP server shape
        "command": sys.executable,
        "args": ["-m", "hive.adapters.claude_channel_server"],
        "env": {"HIVE_HOME": str(_hive_home()), "PYTHONPATH": _child_pythonpath()},
    }


def mcp_config_path() -> Path:
    return _channel_dir() / "mcp-config.json"


def prepare_pane(cwd: str) -> list[str]:
    """Write the static channel MCP config and return Claude launch flags.

    The config lives under ``$HIVE_HOME/channel`` and is passed with
    ``--mcp-config``: no project file is created or modified, so any cwd --
    repo or not, tracked ``.mcp.json`` or not -- works identically. The write
    is an idempotent overwrite (content depends only on the running hive).
    Returns ``[]`` only when the config cannot be written (disk error), which
    the caller must treat as channel-unavailable.

    Both returned flags are variadic in Claude's CLI parser: a positional
    prompt directly after them is consumed as a flag value and aborts launch.
    ``--mcp-config <path>`` is kept last so the flag list is safe to follow
    with another ``--flag``; a positional must be separated with ``--``.
    """
    del cwd  # location-independent since the config left the project tree
    directory = _channel_dir()
    cfg_path = mcp_config_path()
    try:
        directory.mkdir(parents=True, exist_ok=True)
        os.chmod(directory, 0o700)
        cfg_path.write_text(json.dumps(
            {"mcpServers": {SERVER_NAME: _server_entry()}}, indent=2) + "\n")
    except OSError as e:
        sys.stderr.write(
            f"[hive-channel] cannot write {cfg_path}: {e}. "
            f"This claude pane will not receive hive messages.\n")
        return []
    return [
        "--dangerously-load-development-channels", f"server:{SERVER_NAME}",
        "--mcp-config", str(cfg_path),
    ]


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
