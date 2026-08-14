"""Delivery into a Claude session whose argv hive does not own.

Claude Code binds one unix socket per session — its cross-session messaging
inbox — and exports the path to every child process as
``CLAUDE_CODE_MESSAGING_SOCKET``. Any local process may connect and write a
single newline-terminated JSON message; the session queues it as an attributed
peer message and takes it up at its next turn boundary. That is the transport for a
member hive did not launch: the Claude Code desktop app owns its argv, so
neither ``--channels`` (channel push registers only for sessions launched with
the flag) nor a pane running the CLI is available there.

Two properties the anchor-pane design leans on:

- **Discovery is the environment.** ``hive duo init`` runs as a child of the
  session it is forming a duo for, so the socket path is handed to it. It
  follows the session across restarts (the path is pid-keyed) with no probe,
  no registry, and no path formula of ours to go stale.
- **Liveness is the connect.** The session unlinks its socket on exit, and a
  socket file left by a killed session refuses connections. Either way the
  send fails closed and the message stays durable on the bus.

The receiving session's own gate decides whether an inbound peer message
reaches the model: while the session runs in a bypass permission mode and the
sender does not attest a matching mode, the message is *held* for a human
click. Hive does not forge that attestation — it is the receiver's safety
boundary, not ours — so :func:`inbound_accepted` preflights the user's
``crossSessionInbound`` setting instead, the same way the channel path
preflights the managed channels allowlist.
"""
from __future__ import annotations

import json
import os
import re
import socket
from pathlib import Path

ENV_SOCKET = "CLAUDE_CODE_MESSAGING_SOCKET"
SETTING = "crossSessionInbound"

_HIVE_FROM_RE = re.compile(r"<HIVE\b[^>]*\bfrom=([^\s>]+)")

# tmux pane option holding an anchored member's inbox socket path. Pane-keyed
# like every other member authority, so routing/reaping/doctor keep working.
ENDPOINT_OPTION = "hive-remote-endpoint"

# Accepted-transport classification: the message was written to the session's
# inbox socket and a listener accepted the connection. It claims exactly that
# — not that the session's inbound gate released it to the model, and not that
# the model processed it. Transcript confirmation remains the only proof.
ACCEPTED_UDS_WRITE = "udsWriteAccepted"

_CONNECT_TIMEOUT = 2.0
SUBMIT_TIMEOUT = _CONNECT_TIMEOUT

_SETTING_HINT = (
    'set "%s": "accept" in ~/.claude/settings.json (inbound peer messages are '
    "otherwise held for a click while the session bypasses permission prompts)"
    % SETTING
)


def session_socket() -> str:
    """The inbox socket of the Claude session hosting this process.

    Empty when this process is not a child of one — a plain shell, or a host
    that does not run the cross-session messaging server.
    """
    return os.environ.get(ENV_SOCKET, "")


def is_live(sock_path: str | Path) -> bool:
    """Whether something is listening on *sock_path*.

    A connect, never an ``exists()``: a socket file outliving a killed session
    would otherwise read as a live member forever.
    """
    if not sock_path:
        return False
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(_CONNECT_TIMEOUT)
    try:
        conn.connect(str(sock_path))
        return True
    except OSError:
        return False
    finally:
        conn.close()


def _user_settings_path() -> Path:
    root = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    return Path(root) / "settings.json"


def inbound_accepted() -> bool:
    """Whether the user's settings accept inbound peer messages without a click.

    Reads the user settings file only — the one the setup hint tells the user
    to edit. A repository or managed setting may still tighten it back to
    ``hold``; that surfaces as a visible held-message prompt in the session
    rather than as silence, so it needs no preflight of its own.
    """
    try:
        data = json.loads(_user_settings_path().read_text())
    except (OSError, ValueError):
        return False
    return isinstance(data, dict) and data.get(SETTING) == "accept"


def setting_hint() -> str:
    return _SETTING_HINT


def send(sock_path: str | Path, text: str) -> str | None:
    """Deliver *text* to the session listening on *sock_path*.

    Returns :data:`ACCEPTED_UDS_WRITE`, or ``None`` on any transport failure
    (no path, nothing listening, write error) — there is no keystroke
    fallback, so ``Agent.send`` raises that as a delivery failure.
    """
    if not sock_path:
        return None
    # `from` is what the receiving session shows the human as the sender, so it
    # names the hive member rather than defaulting to "unknown". It is not a
    # reply address: the session answers held/denied/delivered receipts to a
    # `uds:<path>` in its own socket namespace, and hive has no socket there.
    # ponytail: fire-and-forget; add a listener if a duo ever needs to tell
    # held from delivered.
    sender = _HIVE_FROM_RE.search(text)
    payload = json.dumps(
        {
            "type": "user",
            # Queued, not preempting: the message waits for the turn in flight
            # instead of cutting into it. A peer does not get to derail a
            # member mid-task; it gets the floor as soon as that task is done.
            "priority": "next",
            "from": f"hive:{sender.group(1)}" if sender else "hive",
            "message": {"role": "user", "content": text},
        }
    )
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(_CONNECT_TIMEOUT)
    try:
        conn.connect(str(sock_path))
        conn.sendall((payload + "\n").encode("utf-8"))
        return ACCEPTED_UDS_WRITE
    except OSError:
        return None
    finally:
        conn.close()
