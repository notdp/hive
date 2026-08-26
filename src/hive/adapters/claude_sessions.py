"""Claude Code sessions on this machine and their cross-session inboxes.

Every Claude Code session with cross-session messaging (2.1.224+) registers
itself in ``<claude-config>/sessions/<pid>.json`` and binds an inbox socket
(``messagingSocketPath``); ``/list-agents`` reads the same files. One line of
JSON written to that socket is queued for the session as a peer message and
read between tool calls (or starts a turn when the session is idle). That is
how a hive member reaches a session that is not on the team — the desktop app,
another terminal — and the only such path a codex member has: it carries no
``SendMessage`` tool.

The registry layout and the inbox line shape are what Claude Code does today
(observed on 2.1.237), not a published contract. Every read here is defensive,
and :func:`send` claims only that the socket accepted the bytes: whether
the session's ``crossSessionInbound`` setting then delivers or holds it is the
receiving session's decision, invisible from here.
"""
from __future__ import annotations

import json
import os
import socket
from dataclasses import dataclass
from pathlib import Path

ACCEPTED_UDS_WRITE = "udsWriteAccepted"
# The listener accepted the connection but did not read the whole frame in
# time — a stalled session, not an absent one; the frame may sit truncated on
# its side.
WRITE_TIMED_OUT = "udsWriteTimedOut"
_CONNECT_TIMEOUT = 2.0
_WRITE_TIMEOUT = 10.0
# The sidecar submit budget must cover a full send() worst case.
SUBMIT_TIMEOUT = _CONNECT_TIMEOUT + _WRITE_TIMEOUT


# Transcript bytes scanned for the desktop title: the `custom-title` record is
# written when the title is set and re-emitted near the tail as the session
# runs, so the tail window finds the current title; the head window catches a
# title set once at the start of a short session.
_TITLE_TAIL_BYTES = 512 * 1024
_TITLE_HEAD_BYTES = 64 * 1024


@dataclass(frozen=True)
class ClaudeSession:
    name: str
    pid: int
    cwd: str
    kind: str
    socket_path: str
    session_id: str = ""
    title: str = ""

    def answers_to(self, label: str) -> bool:
        """*label* is this session's Claude Code name, its desktop title, or
        its pid (the one address that is always unique)."""
        return (
            label == self.name
            or (bool(self.title) and label == self.title)
            or label == str(self.pid)
        )


def _config_dir() -> Path:
    # CLAUDE_HOME is hive's own sandbox lever (tests and dev lanes point it at
    # a disposable tree, see adapters/claude.py); CLAUDE_CONFIG_DIR is Claude
    # Code's relocation knob. Honour both so a sandboxed run never reads — or
    # messages — the developer's real sessions.
    return Path(
        os.environ.get("CLAUDE_HOME")
        or os.environ.get("CLAUDE_CONFIG_DIR")
        or os.path.expanduser("~/.claude")
    )


def _registry_dir() -> Path:
    return _config_dir() / "sessions"


def _title_in(chunk: bytes) -> str:
    title = ""
    for line in chunk.splitlines():
        if b'"custom-title"' not in line:
            continue
        try:
            rec = json.loads(line)
        except ValueError:
            continue  # a partial line at a window edge
        if isinstance(rec, dict) and rec.get("type") == "custom-title":
            title = str(rec.get("customTitle") or "")  # the last record wins, a cleared title included
    return title


def session_title(session_id: str) -> str:
    """The desktop app's title for *session_id* ("" when none was set).

    Claude Code records it in the session transcript as a ``custom-title``
    line, so the title lives beside the conversation, not in the registry.
    """
    if not session_id:
        return ""
    matches = list((_config_dir() / "projects").glob(f"*/{session_id}.jsonl"))
    if not matches:
        return ""
    path = matches[0]
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            fh.seek(max(0, size - _TITLE_TAIL_BYTES))
            title = _title_in(fh.read())
            if not title and size > _TITLE_TAIL_BYTES:
                fh.seek(0)
                title = _title_in(fh.read(_TITLE_HEAD_BYTES))
    except OSError:
        return ""
    return title


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


def list_sessions() -> list[ClaudeSession]:
    """Live sessions that bind an inbox socket, sorted by name.

    A registration whose process is gone, or that records no socket (an older
    CLI, bare mode), is not reachable and is left out. Each row carries the
    session's Claude Code name and, when the desktop app set one, its title —
    either addresses the session.
    """
    root = _registry_dir()
    if not root.is_dir():
        return []
    rows: list[ClaudeSession] = []
    for entry in root.glob("*.json"):
        try:
            data = json.loads(entry.read_text())
        except (OSError, ValueError):
            continue
        if not isinstance(data, dict):
            continue
        name = str(data.get("name") or "")
        pid = data.get("pid")
        sock = str(data.get("messagingSocketPath") or "")
        if not name or not isinstance(pid, int) or isinstance(pid, bool) or not sock:
            continue
        if not _pid_alive(pid):
            continue
        session_id = str(data.get("sessionId") or "")
        rows.append(ClaudeSession(
            name=name,
            pid=pid,
            cwd=str(data.get("cwd") or ""),
            kind=str(data.get("kind") or ""),
            socket_path=sock,
            session_id=session_id,
            title=session_title(session_id),
        ))
    rows.sort(key=lambda s: (s.name, s.pid))
    return rows


def session_status(pid: int | None) -> tuple[str, str] | None:
    """(status, waitingFor) reported by the session running as *pid*.

    Real terminal TUI sessions report ``status`` (idle|busy|waiting — an
    observed vocabulary, not a documented enum) in their registry entry;
    headless/desktop-hosted sessions never do. None when the entry is
    missing, the process is dead, or no status is reported.
    """
    if not pid:
        return None
    try:
        data = json.loads((_registry_dir() / f"{pid}.json").read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict) or not _pid_alive(pid):
        return None
    status = data.get("status")
    if status not in ("idle", "busy", "waiting"):
        return None
    return str(status), str(data.get("waitingFor") or "")


def own_socket() -> str:
    """The inbox socket of the Claude session hosting this process ("" when
    this process is not a child of one)."""
    return os.environ.get("CLAUDE_CODE_MESSAGING_SOCKET", "")


def self_session() -> ClaudeSession | None:
    """The registry entry of the Claude session this process runs inside.

    Identity is the socket, never a saved slot: whichever live registration
    names this process's own inbox is us.
    """
    sock = own_socket()
    if not sock:
        return None
    for s in list_sessions():
        if s.socket_path == sock:
            return s
    return None


def resolve(label: str) -> list[ClaudeSession]:
    """Every live session answering to *label* — its Claude Code name (what
    ``/list-agents`` shows), its desktop title, or its pid. The caller decides
    on >1."""
    return [s for s in list_sessions() if s.answers_to(label)]


def rename(sock_path: str | Path, name: str, *, session_id: str = "") -> bool:
    """Ask the session on *sock_path* to take *name* as its own.

    A ``control/rename`` frame is handled at dispatch — immediately, busy or
    idle — and never touches the composer or the transcript. *session_id*,
    when given, must match the target's own id or the frame is silently
    dropped: the guard against a recycled socket path renaming a stranger.
    True means the frame was written, not that the name changed — the caller
    confirms against the registry.
    """
    if not sock_path or not name:
        return False
    payload: dict[str, str] = {"type": "control", "action": "rename", "name": name}
    if session_id:
        payload["session_id"] = session_id
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(_CONNECT_TIMEOUT)
    try:
        conn.connect(str(sock_path))
        conn.settimeout(_WRITE_TIMEOUT)
        conn.sendall((json.dumps(payload) + "\n").encode("utf-8"))
        return True
    except OSError:
        return False
    finally:
        conn.close()


def send(sock_path: str | Path, text: str, *, sender: str) -> str | None:
    """Queue *text* for the session listening on *sock_path*.

    Returns :data:`ACCEPTED_UDS_WRITE`; :data:`WRITE_TIMED_OUT` when the
    session accepted the connection but did not read the frame in time; or
    ``None`` when nothing is listening. ``priority: later`` queues behind the
    whole turn: the message never interjects between tool calls, so a session
    that is working sees it when it stops — and one that is already idle
    takes it straight into its composer, without any interruption chrome.
    *sender* is what the receiving session shows as the
    message's origin; it is not a reply address — a Claude session replies to
    hive members with the hive CLI, never with ``SendMessage``.
    """
    if not sock_path:
        return None
    payload = json.dumps({
        "type": "user",
        "priority": "later",
        "from": sender,
        "message": {"role": "user", "content": text},
    })
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(_CONNECT_TIMEOUT)
    try:
        conn.connect(str(sock_path))
    except OSError:
        conn.close()
        return None
    conn.settimeout(_WRITE_TIMEOUT)
    try:
        conn.sendall((payload + "\n").encode("utf-8"))
        return ACCEPTED_UDS_WRITE
    except socket.timeout:
        return WRITE_TIMED_OUT
    except OSError:
        return None
    finally:
        conn.close()
