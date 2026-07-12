"""Programmatic message delivery for Claude panes via Claude Code "channels".

Mirrors the codex app-server adapter's role for Claude: ``Agent.send`` hands a
``<HIVE>`` envelope to :func:`send_to_pane`, which writes it to a per-pane unix
socket. The channel MCP server (``claude_channel_server``, spawned by Claude
as a stdio MCP child) turns that into a ``notifications/claude/channel``
push -- no tmux send-keys, no composer draft disturbance -- and answers with a
single-byte local MCP-write receipt. Claude delivery is channel-only: a
``None`` from :func:`send_to_pane` is a transport failure that ``Agent.send``
raises as :class:`hive.agent.DeliveryError` (the sidecar projects it to
``injectStatus=failed``); an accepted classification maps to the durable
``queued`` delivery state until the target's transcript confirms the turn.

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
PUBLISHED_REPO = "notdp/hive"
PLUGIN_SPEC = f"plugin:{SERVER_NAME}@{MARKETPLACE_NAME}"
_PLUGIN_REF = f"{SERVER_NAME}@{MARKETPLACE_NAME}"
_PLUGIN_CMD_TIMEOUT = 60
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
    """Written by the channel server once its MCP initialize response has been
    emitted (content advertises receipt capability); cleared by the server on
    exit and by spawn before a fresh launch (stale-marker guard)."""
    return _channel_dir() / f"hive-pane-{_slug(pane)}.ready"


def clear_ready(pane: str) -> None:
    try:
        ready_marker_path(pane).unlink()
    except OSError:
        pass


def is_ready(pane: str) -> bool:
    return ready_marker_path(pane).exists()


# Marker content advertises the server's receipt capability. "2" = the server
# answers each frame with a single-byte local MCP-write receipt; "1" = legacy
# pre-receipt server (accepted on local socket write, old contract — removable
# once every live pane has restarted onto a "2" server). Anything else (empty,
# corrupt, future) fails closed: better a failed send than a guessed boundary.
MARKER_LEGACY = "1"
MARKER_RECEIPT_CAPABLE = "2"

# Accepted-transport classifications for the sidecar's durable delivery
# observations. Neither claims Claude processed the message: per the Channels
# contract, notifications are unacknowledged and may be silently dropped at
# policy/load boundaries. They only name which local boundary was crossed.
ACCEPTED_MCP_WRITE = "mcpWriteAccepted"
ACCEPTED_LEGACY_SOCKET = "legacySocketAccepted"

_RECEIPT_TIMEOUT = 10.0

# Worst-case local submission budget for one send_to_pane call (connect plus
# receipt wait). The sidecar derives its request budgets from this so a valid
# slow acceptance can never outlive the caller's socket timeout.
SUBMIT_TIMEOUT = _SOCKET_CONNECT_TIMEOUT + _RECEIPT_TIMEOUT


def marker_version(pane: str) -> str:
    """Content of the pane's ready marker ('' when absent/unreadable)."""
    try:
        return ready_marker_path(pane).read_text().strip()
    except OSError:
        return ""


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

    A ``directory`` binding is the legacy locally-generated marketplace
    (possibly at a stale path if HIVE_HOME moved -- callers re-point it). A
    github binding at ``PUBLISHED_REPO`` is the published hive marketplace.
    Anything else under that name is a third party we must not overwrite.
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


def _installed_plugin_refs() -> set[str] | None:
    """Plugin refs from installed_plugins.json, ``None`` if uninspectable.

    A local file read keeps the published-branch launch path free of
    subprocesses; any unreadable or unknown shape falls back to the
    self-heal install rather than guessing.
    """
    root = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    try:
        data = json.loads((Path(root) / "plugins" / "installed_plugins.json").read_text())
        plugins = data["plugins"]
        if not isinstance(plugins, dict):
            return None
        return set(plugins)
    except (OSError, ValueError, KeyError, TypeError):
        return None


def prepare_pane(cwd: str) -> list[str]:
    """Converge the hive channel plugin and return Claude launch flags.

    When the ``hive`` marketplace name is bound to the published github
    marketplace (``PUBLISHED_REPO``), presence of the installed plugin is
    checked via a local file read (freshness belongs to Claude's auto-update
    and the plugin's bootstrap hook, never the launch path) and only a
    missing plugin pays a one-time ``install``. Otherwise the
    legacy hive-owned marketplace under ``$HIVE_HOME/channel/marketplace`` is
    used: assets are (re)written, then ``claude plugin marketplace add`` /
    ``install`` / ``update`` run -- each a cheap no-op when already current,
    so the sequence is idempotent and self-recovers after a removed
    marketplace or plugin. A ``directory``-source ``hive`` marketplace left at
    a stale path (HIVE_HOME moved) is re-pointed to the current one. No
    project file is created or modified in any cwd. Returns ``[]`` when the
    channel cannot be converged (asset write failure, a foreign source
    occupying the ``hive`` name, or a failed/timed-out plugin command); the
    caller must treat that as channel-unavailable and fail loudly.

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

    binding = _marketplace_binding()
    if binding is not None and binding[0] != "directory":
        source, location = binding
        # the published identity is exact: github AND notdp/hive -- a url/npm
        # source whose location parses to the same string is still foreign
        if source != "github" or location != PUBLISHED_REPO:
            return _unavailable(
                f"marketplace name '{MARKETPLACE_NAME}' is bound to a foreign "
                f"{source} source ({location}); run `claude plugin "
                f"marketplace remove {MARKETPLACE_NAME}` if that is not hive's")
        # The published marketplace owns the name. Freshness is owned by
        # Claude's startup auto-update and the plugin's bootstrap hook -- the
        # launch path must not update (`plugin update` git-fetches the
        # marketplace and cost 5-10s per launch). Presence is a local file
        # read; only a missing plugin pays a one-time self-heal install.
        if _PLUGIN_REF in (_installed_plugin_refs() or ()):
            return ["--channels", PLUGIN_SPEC]
        out = _claude_plugin("install", _PLUGIN_REF)
        if out is None or out.returncode != 0:
            detail = ((out.stderr or out.stdout).strip()[-300:]
                      if out is not None else "launch failure or timeout")
            return _unavailable(f"`claude plugin install {_PLUGIN_REF}` failed: {detail}")
        return ["--channels", PLUGIN_SPEC]

    # Legacy locally-generated marketplace (retires once the published
    # marketplace is the only registration path).
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
    if binding is not None:
        _source, location = binding
        stale = os.path.realpath(location) != ours
        if stale and not _is_hive_marketplace(location):
            return _unavailable(
                f"marketplace name '{MARKETPLACE_NAME}' is bound to a foreign "
                f"directory source ({location}); run `claude plugin "
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


def send_to_pane(pane: str, text: str) -> str | None:
    """Deliver ``text`` over the pane's channel socket.

    Returns an accepted-transport classification, or ``None`` on transport
    failure (Claude is channel-only — there is no keystroke fallback):

    - ``ACCEPTED_MCP_WRITE``: a marker-``2`` server answered with its
      single-byte local MCP-write receipt, i.e. the notification was written
      and flushed to the MCP stdio transport — the boundary the Channels
      contract defines for ``mcp.notification()``. Not a Claude-processing or
      final-delivery acknowledgement.
    - ``ACCEPTED_LEGACY_SOCKET``: a legacy marker-``1`` server took the frame
      on its local socket (pre-receipt contract); never failed by timeout.
    - ``None``: no/unknown marker (fail-closed), no socket, connect/write
      error, or a marker-``2`` server that did not return exactly the receipt
      byte (server died before the MCP write — the frame must count as lost).
    """
    if not pane:
        return None
    version = marker_version(pane)
    if version not in (MARKER_LEGACY, MARKER_RECEIPT_CAPABLE):
        return None
    sock_path = channel_socket_path(pane)
    if not sock_path.exists():
        return None
    payload = json.dumps({"msg_id": _extract_msg_id(text), "content": text})
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(_SOCKET_CONNECT_TIMEOUT)
    try:
        conn.connect(str(sock_path))
        conn.sendall(payload.encode("utf-8"))
        conn.shutdown(socket.SHUT_WR)
        if version == MARKER_LEGACY:
            return ACCEPTED_LEGACY_SOCKET
        conn.settimeout(_RECEIPT_TIMEOUT)
        receipt = conn.recv(1)
        return ACCEPTED_MCP_WRITE if receipt == b"1" else None
    except OSError:
        return None
    finally:
        conn.close()
