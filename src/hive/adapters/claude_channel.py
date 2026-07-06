"""Programmatic message delivery for Claude panes via Claude Code "channels".

Mirrors the codex app-server adapter's role for Claude: ``Agent.send`` hands a
``<HIVE>`` envelope to :func:`send_to_pane`, which writes it to a per-pane unix
socket. The channel MCP server (``claude_channel_server``, spawned by Claude
as a stdio MCP child) turns that into a ``notifications/claude/channel``
push -- no tmux send-keys, no composer draft disturbance. Claude delivery is
channel-only; on a ``False`` from :func:`send_to_pane`, ``Agent.send`` raises
:class:`ChannelDeliveryError`, which callers surface as an explicit submit
failure (the sidecar projects it to ``injectStatus=failed``).

Channel registration: hive owns a plugin marketplace under
``$HIVE_HOME/channel/marketplace`` whose single plugin declares this MCP
server plus a ``channels`` entry; panes launch with plain
``--channels plugin:hive-channel@hive``. No project file is ever touched and
no consent dialog appears -- but ONLY when the machine's managed settings
allowlist the plugin (``allowedChannelPlugins``). Claude enforces that
allowlist by silently skipping channel notifications: the server runs and the
socket comes up while the session stays deaf, so :func:`prepare_pane`
preflights the policy file and fails loudly with setup instructions instead.
``--channels`` is variadic, so the caller must terminate the flag list (spawn
puts ``--`` before the positional prompt) or append the flags after all
positionals.
"""
from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import sys
import zlib
from pathlib import Path

SERVER_NAME = "hive-channel"
MARKETPLACE_NAME = "hive"
PLUGIN_SPEC = f"plugin:{SERVER_NAME}@{MARKETPLACE_NAME}"
_PLUGIN_REF = f"{SERVER_NAME}@{MARKETPLACE_NAME}"
_PLUGIN_CMD_TIMEOUT = 60
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


def marketplace_dir() -> Path:
    return _channel_dir() / "marketplace"


def _plugin_version(entry: dict) -> str:
    """Deterministic version derived from the server entry: content drift
    (different python, hive import root, HIVE_HOME) yields a new version, so
    ``claude plugin update`` installs it; unchanged content is a no-op."""
    digest = zlib.crc32(json.dumps(entry, sort_keys=True).encode("utf-8"))
    return f"0.1.{digest}"


def _write_plugin_assets() -> None:
    entry = _server_entry()
    root = marketplace_dir()
    description = "hive per-pane message channel for Claude panes"
    (root / ".claude-plugin").mkdir(parents=True, exist_ok=True)
    plugin_meta = root / SERVER_NAME / ".claude-plugin"
    plugin_meta.mkdir(parents=True, exist_ok=True)
    (root / ".claude-plugin" / "marketplace.json").write_text(json.dumps({
        "name": MARKETPLACE_NAME,
        "owner": {"name": "hive"},
        "plugins": [{
            "name": SERVER_NAME,
            "source": f"./{SERVER_NAME}",
            "description": description,
        }],
    }, indent=2) + "\n")
    (plugin_meta / "plugin.json").write_text(json.dumps({
        "name": SERVER_NAME,
        "description": description,
        "version": _plugin_version(entry),
        "mcpServers": {SERVER_NAME: entry},
        "channels": [{"server": SERVER_NAME}],
    }, indent=2) + "\n")


def _unavailable(reason: str) -> list[str]:
    sys.stderr.write(
        f"[hive-channel] {reason}. "
        f"This claude pane will not receive hive messages.\n")
    return []


def _claude_plugin(*args: str) -> subprocess.CompletedProcess | None:
    try:
        return subprocess.run(
            ["claude", "plugin", *args],
            capture_output=True, text=True, timeout=_PLUGIN_CMD_TIMEOUT,
        )
    except (OSError, subprocess.SubprocessError):
        return None


_MANAGED_SETTINGS_PATHS = (
    "/Library/Application Support/ClaudeCode/managed-settings.json",  # macOS
    "/etc/claude-code/managed-settings.json",  # Linux
)

_ALLOWLIST_SETUP_HINT = (
    "sudo mkdir -p \"/Library/Application Support/ClaudeCode\" && "
    "printf '{\\n  \"channelsEnabled\": true,\\n  \"allowedChannelPlugins\": "
    "[\\n    { \"marketplace\": \"hive\", \"plugin\": \"hive-channel\" }\\n  ]"
    "\\n}\\n' | sudo tee "
    "\"/Library/Application Support/ClaudeCode/managed-settings.json\""
)


def _channel_allowlisted() -> bool:
    """Whether the machine's managed settings allowlist the hive channel.

    Claude enforces the channels allowlist: without this entry it loads the
    plugin's MCP server but silently skips channel notifications -- socket and
    marker come up while the session stays deaf. Preflighting the policy file
    turns that silent black hole into a loud setup error."""
    for path in _MANAGED_SETTINGS_PATHS:
        try:
            data = json.loads(Path(path).read_text())
        except (OSError, ValueError):
            continue
        if not isinstance(data, dict):
            continue
        # Require the exact shape the setup hint writes -- the only shape
        # verified to deliver. An allowlist entry without channelsEnabled is
        # unverified territory; refusing loudly beats risking a deaf session.
        if data.get("channelsEnabled") is not True:
            continue
        for entry in data.get("allowedChannelPlugins", []):
            if (isinstance(entry, dict)
                    and entry.get("marketplace") == MARKETPLACE_NAME
                    and entry.get("plugin") == SERVER_NAME):
                return True
    return False


def _is_hive_marketplace(location: str) -> bool:
    """Whether a directory binding at ``location`` is provably hive's own
    (re-pointable after a HIVE_HOME move). A dead path serves nothing and is
    safe to re-point; a live one must carry a readable manifest naming hive
    as owner -- anything unprovable is foreign and never clobbered."""
    root = Path(location)
    if not root.exists():
        return True
    try:
        data = json.loads(
            (root / ".claude-plugin" / "marketplace.json").read_text())
    except (OSError, ValueError):
        return False
    return data.get("owner", {}).get("name") == "hive"


def _marketplace_binding() -> tuple[str, str] | None:
    """``(source, path_or_repo)`` currently bound to hive's marketplace name,
    or ``None`` if the name is free or cannot be inspected.

    hive only ever creates a ``directory``-source marketplace named ``hive``,
    so a directory binding is ours (possibly at a stale path if HIVE_HOME
    moved -- callers re-point it). Any other source under that name is a third
    party we must not overwrite.
    """
    out = _claude_plugin("marketplace", "list", "--json")
    if out is None or out.returncode != 0:
        return None
    try:
        entries = json.loads(out.stdout)
    except ValueError:
        return None
    for e in entries:
        if isinstance(e, dict) and e.get("name") == MARKETPLACE_NAME:
            return str(e.get("source") or ""), str(e.get("path") or e.get("repo") or "")
    return None


def prepare_pane(cwd: str) -> list[str]:
    """Converge the hive channel plugin and return Claude launch flags.

    Registration is a hive-owned plugin marketplace under
    ``$HIVE_HOME/channel/marketplace``: assets are (re)written, then
    ``claude plugin marketplace add`` / ``install`` / ``update`` run -- each a
    cheap no-op when already current, so the sequence is idempotent and
    self-recovers after a removed marketplace or plugin. A ``directory``-source
    ``hive`` marketplace left at a stale path (HIVE_HOME moved) is re-pointed
    to the current one. No project file is created or modified in any cwd.
    Returns ``[]`` when the channel cannot be converged (asset write failure, a
    non-directory source occupying the ``hive`` name, or a failed/timed-out
    plugin command); the caller must treat that as channel-unavailable and
    fail loudly.

    ``--channels`` is variadic in Claude's CLI parser: a positional prompt
    directly after it is consumed as a flag value; a positional must be
    separated with ``--``.
    """
    del cwd  # location-independent: the marketplace lives under $HIVE_HOME
    if not _channel_allowlisted():
        return _unavailable(
            "the hive channel plugin is not on this machine's managed "
            "channels allowlist (claude would load the server but silently "
            "skip channel notifications). One-time setup:\n  "
            + _ALLOWLIST_SETUP_HINT)
    try:
        _write_plugin_assets()
    except OSError as e:
        return _unavailable(f"cannot write plugin assets under {marketplace_dir()}: {e}")

    ours = os.path.realpath(str(marketplace_dir()))
    steps: list[tuple[str, ...]] = [
        ("marketplace", "add", ours),
        ("install", _PLUGIN_REF),
        ("update", _PLUGIN_REF),
    ]
    binding = _marketplace_binding()
    if binding is not None:
        source, location = binding
        stale = os.path.realpath(location) != ours
        if source != "directory" or (stale and not _is_hive_marketplace(location)):
            return _unavailable(
                f"marketplace name '{MARKETPLACE_NAME}' is bound to a foreign "
                f"{source} source ({location}); run `claude plugin "
                f"marketplace remove {MARKETPLACE_NAME}` if that is not hive's")
        if stale:
            # hive's own binding at a stale path (HIVE_HOME moved) -> re-point
            steps.insert(0, ("marketplace", "remove", MARKETPLACE_NAME))

    for step in steps:
        out = _claude_plugin(*step)
        if out is None or out.returncode != 0:
            detail = ((out.stderr or out.stdout).strip()[-300:]
                      if out is not None else "launch failure or timeout")
            return _unavailable(f"`claude plugin {' '.join(step)}` failed: {detail}")
    return ["--channels", PLUGIN_SPEC]


# --- delivery ---------------------------------------------------------------

def _extract_msg_id(text: str) -> str:
    m = _MSGID_RE.search(text)
    return m.group(1) if m else ""


def send_to_pane(pane: str, text: str) -> bool:
    """Deliver ``text`` over the pane's channel socket.

    Returns ``False`` (a delivery failure -- Claude is channel-only, so there is
    no keystroke fallback) when the channel is not locally ready: no ready marker
    (channel never registered), no socket, or a refused/timed-out connect. A
    successful write returns ``True``; the ready marker -- written by the
    channel server once its socket listens -- is what distinguishes a live
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
