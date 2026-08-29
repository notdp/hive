"""Codex app-server client over a single shared daemon.

One ``codex app-server --listen unix://<sock>`` daemon per CODEX_HOME hosts
every hive codex thread. Each codex TUI attaches with ``codex resume
<threadId> --remote unix://<sock> --cd <cwd>`` and drives its own thread;
hive connects as one more client over the same socket for runtime signals and
turn delivery.

Identity is the threadId (== transcript sessionId), never the process
environment: the daemon's env is frozen at spawn time and shared by every
thread, so ``TMUX_PANE`` is stripped from it and codex's own per-thread
``CODEX_THREAD_ID`` injection into tool subprocesses is the tool-side identity.
Which thread belongs to which tmux pane is recorded in a per-pane ``.thread``
file beside the socket, written by whoever binds the pane to a thread (spawn,
managed launch, fork). "Latest thread" heuristics are unusable on a shared
daemon and do not exist here.

Spawn primitive (0.149.0 real-machine verified): ``thread/start`` (with cwd,
optionally model) creates the thread but does not persist it; a follow-up
``thread/name/set`` flushes the rollout to disk, after which both the TUI's
``codex resume <threadId>`` and any client's ``thread/resume`` succeed.
``thread/fork`` forks a rolled-out thread server-side and returns the new
thread.

Broadcast surface for a second client (verified): ``thread/status/changed``
(active/idle with activeFlags) and ``thread/goal/*``. ``turn/*`` and ``item/*``
notifications are delivered only to the client that started the turn, so all
busy/inputState state folds from status events alone.

Directory trust in remote mode is judged from the ``[projects]`` entries in the
daemon's config.toml on disk (``-c`` overrides do not apply), so every new cwd
is trusted via :func:`ensure_dir_trusted` before its thread starts.

Transport is WebSocket framing over the unix socket — stdlib-only RFC6455
masked text frames, one background reader thread per connection.
"""

from __future__ import annotations

import base64
import json
import os
import re
import socket
import struct
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path

_HANDSHAKE_TIMEOUT = 5.0
_CALL_TIMEOUT = 10.0

# Worst-case local submission budget for one send_to_pane call (fresh daemon
# handshake plus the turn/start RPC). The sidecar derives its request budgets
# from this so a valid slow acceptance can never outlive the caller's timeout.
SUBMIT_TIMEOUT = _HANDSHAKE_TIMEOUT + _CALL_TIMEOUT
_DAEMON_START_TIMEOUT = 8.0
_CONNECT_COOLDOWN = 5.0
_RESUME_COOLDOWN = 5.0


def codex_home() -> Path:
    return Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))


def shared_socket_path() -> Path:
    """The shared daemon's socket under the real CODEX_HOME.

    Lives under ``app-server-control/`` (a real directory codex itself uses, so
    it is never a symlink — codex rejects a symlinked socket parent, e.g.
    ``/tmp`` on macOS). The path carries no per-pane or per-worktree component:
    unix socket paths cap at ~104 bytes (SUN_LEN) and there is exactly one
    daemon per CODEX_HOME.
    """
    return codex_home() / "app-server-control" / "hive-shared.sock"


def shared_pidfile_path() -> Path:
    return shared_socket_path().with_suffix(".pid")


def pane_thread_path(pane: str) -> Path:
    """Per-pane record of the thread hive bound to this pane."""
    slug = pane.replace("%", "") or "default"
    return codex_home() / "app-server-control" / f"hive-pane-{slug}.thread"


def write_pane_thread(pane: str, thread_id: str, cwd: str) -> None:
    path = pane_thread_path(pane)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"threadId": thread_id, "cwd": cwd}))


def read_pane_thread(pane: str) -> tuple[str, str] | None:
    try:
        data = json.loads(pane_thread_path(pane).read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    thread_id, cwd = data.get("threadId"), data.get("cwd")
    if not thread_id:
        return None
    return str(thread_id), str(cwd or "")


def clear_pane_thread(pane: str) -> None:
    pane_thread_path(pane).unlink(missing_ok=True)


def thread_id_for_pane(pane: str) -> str | None:
    record = read_pane_thread(pane)
    return record[0] if record else None


def _pane_from_record_name(name: str) -> str | None:
    """Inverse of :func:`pane_thread_path`: ``hive-pane-19.thread`` -> ``%19``."""
    if not name.startswith("hive-pane-") or not name.endswith(".thread"):
        return None
    slug = name[len("hive-pane-"):-len(".thread")]
    if not slug or slug == "default":
        return None
    return "%" + slug


def list_recorded_panes() -> list[str]:
    """Pane ids that currently have a thread record on disk."""
    root = codex_home() / "app-server-control"
    if not root.is_dir():
        return []
    panes: list[str] = []
    for entry in root.glob("hive-pane-*.thread"):
        pane = _pane_from_record_name(entry.name)
        if pane:
            panes.append(pane)
    return panes


def pane_for_thread(thread_id: str) -> str | None:
    """Pane recorded for *thread_id*, or None.

    The reverse lookup behind tool-side identity: a ``hive`` invocation inside
    a codex tool carries ``CODEX_THREAD_ID`` (injected per thread by codex),
    and this maps it back to the tmux pane hive bound the thread to.
    """
    if not thread_id:
        return None
    for pane in list_recorded_panes():
        record = read_pane_thread(pane)
        if record and record[0] == thread_id:
            return pane
    return None


# --------------------------------------------------------------------------
# directory trust (config.toml)
# --------------------------------------------------------------------------
_TRUST_LEVEL_RE = re.compile(r"^\s*trust_level\s*=")


def _trusted_section_headers(directory: str) -> tuple[str, ...]:
    """Header spellings that name *directory*'s [projects] entry.

    Codex writes the TOML basic-string form; the literal-string form is also
    matched (when representable) so a hand-edited entry is not duplicated —
    a duplicate table would make the whole config.toml unparsable.
    """
    escaped = directory.replace("\\", "\\\\").replace('"', '\\"')
    headers = [f'[projects."{escaped}"]']
    if "'" not in directory:
        headers.append(f"[projects.'{directory}']")
    return tuple(headers)


def ensure_dir_trusted(directory: str) -> None:
    """Converge ``[projects."<dir>"] trust_level = "trusted"`` in config.toml.

    Remote-mode directory trust is judged from the daemon's config.toml on
    disk (``-c`` overrides do not apply), so every new cwd must be trusted
    before its thread starts. Idempotent line-level edit in the same spirit as
    ``core_hooks._ensure_codex_hooks_enabled``: read, minimally patch, write
    only on change; an unreadable config is left alone.
    """
    config_path = codex_home() / "config.toml"
    content = ""
    if config_path.exists():
        try:
            content = config_path.read_text()
        except OSError:
            return
    original = content
    headers = _trusted_section_headers(directory)
    lines = content.splitlines(keepends=True)
    start = None
    for i, line in enumerate(lines):
        stripped = line.strip()
        if any(stripped == h or stripped.startswith(h + " ") or stripped.startswith(h + "#") for h in headers):
            start = i + 1
            break
    if start is None:
        section = f'{headers[0]}\ntrust_level = "trusted"\n'
        if not content:
            content = section
        elif content.endswith("\n"):
            content += "\n" + section
        else:
            content += "\n\n" + section
    else:
        end = start
        while end < len(lines) and not lines[end].strip().startswith("["):
            end += 1
        body = lines[start:end]
        replaced = False
        for j, line in enumerate(body):
            if _TRUST_LEVEL_RE.match(line):
                if line.strip() == 'trust_level = "trusted"':
                    return
                body[j] = 'trust_level = "trusted"\n'
                replaced = True
                break
        if not replaced:
            body.insert(0, 'trust_level = "trusted"\n')
        lines[start:end] = body
        content = "".join(lines)
    if content != original:
        config_path.parent.mkdir(parents=True, exist_ok=True)
        config_path.write_text(content)


# --------------------------------------------------------------------------
# transport: minimal RFC6455 client over a unix socket (text frames, masked)
# --------------------------------------------------------------------------
# Accepted-transport classification for durable delivery observations: the
# shared daemon took the turn. Not proof the turn produced output.
TURN_START_ACCEPTED = "turnStartAccepted"

# Interrupt outcomes: the daemon aborted the running turn, or there was no
# turn to abort (an idle thread is nothing to interrupt, not a failure).
TURN_INTERRUPT_ACCEPTED = "turnInterruptAccepted"
NO_RUNNING_TURN = "noRunningTurn"

class _WSConn:
    def __init__(self, path: str, timeout: float = _HANDSHAKE_TIMEOUT):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self._rx = b""
        self._handshake()
        # The timeout guards only the handshake. A live daemon can legally go
        # silent for 5s+ mid-call (its models refresh stalls exactly 5.00s on
        # a stale cache), and socket.timeout is an OSError — leaving it armed
        # lets that silence kill the reader thread right before the response.
        self.sock.settimeout(None)

    def _handshake(self) -> None:
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            "GET / HTTP/1.1\r\nHost: localhost\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("app-server handshake closed early")
            data += chunk
        if b"101" not in data.split(b"\r\n", 1)[0]:
            raise ConnectionError(f"app-server handshake rejected: {data[:64]!r}")
        self._rx = data.split(b"\r\n\r\n", 1)[1]

    def _recv_exact(self, n: int) -> bytes:
        while len(self._rx) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("app-server connection closed")
            self._rx += chunk
        out, self._rx = self._rx[:n], self._rx[n:]
        return out

    def _recv_frame(self) -> tuple[bool, int, bytes]:
        b0, b1 = self._recv_exact(2)
        fin = bool(b0 & 0x80)
        opcode = b0 & 0x0F
        masked = b1 & 0x80
        length = b1 & 0x7F
        if length == 126:
            (length,) = struct.unpack(">H", self._recv_exact(2))
        elif length == 127:
            (length,) = struct.unpack(">Q", self._recv_exact(8))
        mask = self._recv_exact(4) if masked else b""
        payload = self._recv_exact(length)
        if masked:
            payload = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
        return fin, opcode, payload

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        n = len(payload)
        header = bytes([0x80 | opcode])
        if n < 126:
            header += bytes([0x80 | n])
        elif n < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = os.urandom(4)
        masked = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def recv_text(self) -> str:
        buf = b""
        while True:
            fin, opcode, payload = self._recv_frame()
            if opcode == 0x8:
                raise ConnectionError("app-server sent close")
            if opcode == 0x9:
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            buf += payload
            if fin:
                return buf.decode("utf-8", "replace")

    def send_text(self, text: str) -> None:
        self._send_frame(0x1, text.encode())

    def close(self) -> None:
        try:
            self._send_frame(0x8, b"")
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass


# --------------------------------------------------------------------------
# per-thread runtime state, kept current by the reader thread
# --------------------------------------------------------------------------
@dataclass
class ThreadRuntime:
    busy: bool = False
    turn_phase: str = "unknown_evidence"
    input_state: str = ""
    observed_at: float = 0.0


def _apply_status(rt: ThreadRuntime, status: dict) -> None:
    kind = status.get("type")
    if kind == "active":
        rt.busy = True
        rt.turn_phase = "tool_open"
        flags = status.get("activeFlags") or []
        if "waitingOnApproval" in flags or "waitingOnUserInput" in flags:
            rt.input_state = "waiting_user"
        else:
            rt.input_state = "ready"
    elif kind == "idle":
        rt.busy = False
        rt.turn_phase = "turn_closed"
        rt.input_state = "ready"
    # notLoaded / systemError: leave prior fields, only observed_at advanced


# --------------------------------------------------------------------------
# one connection to the shared daemon
# --------------------------------------------------------------------------
class CodexDaemonClient:
    def __init__(self, socket_path: str):
        self.socket_path = socket_path
        self._conn = _WSConn(socket_path)
        self._send_lock = threading.Lock()
        self._state_lock = threading.Lock()
        self._pending: dict[int, dict] = {}
        self._threads: dict[str, ThreadRuntime] = {}
        self._resume_cooldown: dict[str, float] = {}
        self._id = 0
        self._closed = False
        self._reader = threading.Thread(target=self._reader_loop, daemon=True)
        self._reader.start()

    # ---- request/response ----
    def call(self, method: str, params: dict | None = None, timeout: float = _CALL_TIMEOUT) -> dict:
        with self._send_lock:
            if self._closed:
                return {"__error__": "closed"}
            self._id += 1
            rid = self._id
            slot = {"event": threading.Event(), "msg": None}
            self._pending[rid] = slot
            try:
                self._conn.send_text(json.dumps({"id": rid, "method": method, "params": params or {}}))
            except OSError as exc:
                self._pending.pop(rid, None)
                return {"__error__": str(exc)}
        if not slot["event"].wait(timeout):
            self._pending.pop(rid, None)
            return {"__timeout__": True}
        msg = slot["msg"] or {}
        if "error" in msg:
            return {"__error__": msg["error"]}
        return {"result": msg.get("result")}

    def _reader_loop(self) -> None:
        while not self._closed:
            try:
                txt = self._conn.recv_text()
            except (OSError, ConnectionError):
                break
            try:
                msg = json.loads(txt)
            except ValueError:
                continue
            rid = msg.get("id")
            # Pop atomically: a `call()` that timed out concurrently may have
            # already removed this rid, so a check-then-pop would race into a
            # KeyError and kill the reader thread. A missing slot just means the
            # waiter is gone (timed out) — drop the late response silently.
            slot = self._pending.pop(rid, None) if rid is not None else None
            if slot is not None:
                slot["msg"] = msg
                slot["event"].set()
            elif msg.get("method"):
                self._on_notification(msg["method"], msg.get("params") or {})
        self._closed = True

    # ---- notification -> state ----
    def _on_notification(self, method: str, params: dict) -> None:
        # thread/status/changed is the only busy-relevant notification a
        # non-turn-owning client receives on the shared daemon (turn/* and
        # item/* go to the turn's own client only).
        if method != "thread/status/changed":
            return
        tid = params.get("threadId")
        if not tid:
            return
        with self._state_lock:
            rt = self._threads.setdefault(tid, ThreadRuntime())
            rt.observed_at = time.time()
            _apply_status(rt, params.get("status") or {})

    def _seed_status(self, thread_id: str, status: object) -> None:
        if not isinstance(status, dict):
            return
        with self._state_lock:
            rt = self._threads.setdefault(thread_id, ThreadRuntime())
            rt.observed_at = time.time()
            _apply_status(rt, status)

    def runtime_for(self, thread_id: str) -> ThreadRuntime | None:
        with self._state_lock:
            rt = self._threads.get(thread_id)
            return ThreadRuntime(**rt.__dict__) if rt is not None else None

    def runtime_or_backfill(self, thread_id: str) -> ThreadRuntime | None:
        """Runtime for *thread_id*, resuming once to recover missing state.

        A client connected before the thread existed has no state for it until
        the first status broadcast; ``thread/resume`` returns the thread's
        current status and backfills it. Rate-limited per thread so a
        never-resolving id does not storm resumes.
        """
        rt = self.runtime_for(thread_id)
        if rt is not None:
            return rt
        with self._state_lock:
            if time.monotonic() < self._resume_cooldown.get(thread_id, 0.0):
                return None
            self._resume_cooldown[thread_id] = time.monotonic() + _RESUME_COOLDOWN
        self.resume(thread_id)
        return self.runtime_for(thread_id)

    # ---- protocol helpers ----
    def initialize(self) -> bool:
        res = self.call("initialize", {
            "clientInfo": {"name": "hive", "title": "hive", "version": "1"},
            "capabilities": {"experimentalApi": True},
        })
        return "result" in res

    def attach(self) -> None:
        """Recover state for already-active threads (busy late-join).

        A client online when a status edge fires gets the broadcast; this
        covers the late-join case by resuming each loaded thread once — the
        resume response carries the thread's current status.
        """
        for tid in self.loaded_list():
            self.resume(tid)

    def loaded_list(self) -> list[str]:
        res = self.call("thread/loaded/list", {})
        return (res.get("result") or {}).get("data") or [] if "result" in res else []

    def resume(self, thread_id: str) -> bool:
        """Backfill a thread's current status from ``thread/resume``."""
        res = self.call("thread/resume", {"threadId": thread_id, "excludeTurns": True})
        result = res.get("result")
        if not isinstance(result, dict):
            return False
        thread = result.get("thread")
        if isinstance(thread, dict):
            self._seed_status(thread_id, thread.get("status"))
        return True

    def start_thread(self, cwd: str, *, name: str, model: str = "") -> str | None:
        """Mint a new thread for *cwd*; return its threadId (== sessionId).

        ``thread/start`` alone leaves the thread unpersisted — ``thread/resume``
        (and therefore the TUI's ``codex resume <tid>``) fails with ``no
        rollout found``. The follow-up ``thread/name/set`` flushes the rollout
        to disk (0.149.0 verified), so a minted thread is immediately
        resumable. *name* must be non-empty (the daemon rejects empty names).
        """
        params: dict = {"cwd": cwd}
        if model:
            params["model"] = model
        res = self.call("thread/start", params)
        result = res.get("result")
        if not isinstance(result, dict):
            return None
        thread = result.get("thread")
        if not isinstance(thread, dict) or not thread.get("id"):
            return None
        tid = str(thread["id"])
        self._seed_status(tid, thread.get("status"))
        if "result" not in self.call("thread/name/set", {"threadId": tid, "name": name}):
            return None  # unflushed thread is not attachable; treat as failure
        return tid

    def fork_thread(self, thread_id: str, *, name: str) -> str | None:
        """Fork a rolled-out thread server-side; return the fork's threadId."""
        res = self.call("thread/fork", {"threadId": thread_id})
        result = res.get("result")
        if not isinstance(result, dict):
            return None
        thread = result.get("thread")
        if not isinstance(thread, dict) or not thread.get("id"):
            return None
        tid = str(thread["id"])
        self._seed_status(tid, thread.get("status"))
        if "result" not in self.call("thread/name/set", {"threadId": tid, "name": name}):
            return None
        return tid

    def turn_start(self, thread_id: str, text: str) -> dict:
        return self.call("turn/start", {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
        })

    def active_turn_id(self, thread_id: str) -> str | None:
        """Id of the thread's in-progress turn, read from the daemon.

        ``turn/interrupt`` requires the turnId and ``ThreadStatus::Active``
        carries none, so the id has to be read back — hive never owns the
        turn (the pane's TUI started it) and only the starting client gets
        ``turn/*`` notifications. ``thread/read`` with ``includeTurns`` is the
        one route: there is no ``thread/turns/list`` on this surface.
        """
        res = self.call("thread/read", {"threadId": thread_id, "includeTurns": True})
        result = res.get("result")
        if not isinstance(result, dict):
            return None
        thread = result.get("thread")
        turns = thread.get("turns") if isinstance(thread, dict) else None
        for turn in reversed(turns or []):
            if isinstance(turn, dict) and turn.get("status") == "inProgress":
                return str(turn.get("id") or "") or None
        return None

    def turn_interrupt(self, thread_id: str, turn_id: str) -> dict:
        """Abort *turn_id* on *thread_id*.

        0.149.1 verified: the turnId is mandatory (omitting it answers
        ``-32600 Invalid request: missing field turnId``) and is checked
        against the live turn (``-32600 expected active turn id <x> but found
        <y>``), so a stale id can never abort a turn that started since.
        """
        return self.call("turn/interrupt", {"threadId": thread_id, "turnId": turn_id})

    def compact_start(self, thread_id: str) -> dict:
        """Start a context-compaction turn (the ``/compact`` slash equivalent).

        This is the dedicated RPC the codex TUI fires for ``/compact``; sending
        ``/compact`` as ``turn/start`` text only feeds the model a literal
        prompt and never compacts.
        """
        return self.call("thread/compact/start", {"threadId": thread_id})

    def is_alive(self) -> bool:
        return not self._closed and self._reader.is_alive()

    def close(self) -> None:
        self._closed = True
        self._conn.close()


# --------------------------------------------------------------------------
# daemon lifecycle
# --------------------------------------------------------------------------
def probe_socket(socket_path: str) -> bool:
    """True when a live daemon answers initialize on this socket."""
    try:
        conn = _WSConn(socket_path, timeout=2.0)
    except OSError:
        return False
    try:
        conn.send_text(json.dumps({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "hive-probe", "version": "0"},
        }}))
        txt = conn.recv_text()
        return json.loads(txt).get("id") == 1
    except (OSError, ConnectionError, ValueError):
        return False
    finally:
        conn.close()


def daemon_alive() -> bool:
    sock = shared_socket_path()
    return sock.exists() and probe_socket(str(sock))


def _daemon_env() -> dict[str, str]:
    """Daemon env: the shared daemon serves every pane, so per-pane identity
    markers must not freeze into it — tool subprocesses inherit this env and a
    stale TMUX_PANE would impersonate whichever pane spawned the daemon.
    Identity rides codex's own per-thread CODEX_THREAD_ID injection instead.

    CLAUDE*/ANTHROPIC* are washed for the same reason (as the grok leader
    does): the spawner may itself run inside a claude engine, and an inherited
    CLAUDE_CODE_MESSAGING_SOCKET makes every hive call from a codex tool shell
    resolve to *that* engine's pane whenever the thread lookup misses."""
    env = {
        k: v for k, v in os.environ.items()
        if not (k.startswith("CLAUDE") or k.startswith("ANTHROPIC"))
    }
    env.pop("TMUX_PANE", None)
    env.pop("HIVE_CODEX_PANE", None)
    return env


def spawn_daemon(*, codex_bin: str = "codex", timeout: float = _DAEMON_START_TIMEOUT) -> bool:
    """Ensure the shared app-server daemon is listening; return True if ready.

    Reuses a live daemon if one already answers on the shared socket
    (idempotent spawn); a stale socket from a dead daemon is removed first.
    Shares the real CODEX_HOME (auth/model/permission defaults stay correct).
    ``start_new_session`` detaches it from the short-lived caller. The daemon
    is machine-level state: nothing in hive kills it when panes or teams go
    away, and the sidecar re-spawns it if it dies while codex members live.
    Returns False if the daemon fails to bind or dies before becoming ready.
    """
    sock = shared_socket_path()
    sock.parent.mkdir(parents=True, exist_ok=True)
    if sock.exists():
        if probe_socket(str(sock)):
            return True  # reuse the live daemon
        try:
            sock.unlink()  # stale socket from a dead daemon
        except OSError:
            pass
    try:
        proc = subprocess.Popen(
            [codex_bin, "app-server", "--listen", f"unix://{sock}"],
            env=_daemon_env(),
            stdout=subprocess.DEVNULL,
            stderr=open(codex_home() / "app-server-control" / "daemon.stderr", "ab"),
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
    except (OSError, ValueError):
        return False
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return False  # died before binding
        if probe_socket(str(sock)):
            try:
                shared_pidfile_path().write_text(str(proc.pid))
            except OSError:
                pass
            return True
        time.sleep(0.2)
    try:
        proc.terminate()
    except OSError:
        pass
    return False


# --------------------------------------------------------------------------
# shared client (one per process, lazily connected)
# --------------------------------------------------------------------------
_CLIENT: CodexDaemonClient | None = None
_CLIENT_LOCK = threading.Lock()
_CLIENT_COOLDOWN_UNTIL = 0.0


def _shared_client() -> CodexDaemonClient | None:
    global _CLIENT, _CLIENT_COOLDOWN_UNTIL
    with _CLIENT_LOCK:
        if _CLIENT is not None and _CLIENT.is_alive():
            return _CLIENT
        if _CLIENT is not None:
            _CLIENT.close()
            _CLIENT = None
        if time.monotonic() < _CLIENT_COOLDOWN_UNTIL:
            return None

    sock = shared_socket_path()
    if not sock.exists():
        _set_cooldown()
        return None
    client: CodexDaemonClient | None = None
    try:
        client = CodexDaemonClient(str(sock))
        if not client.initialize():
            raise ConnectionError("initialize failed")
    except (OSError, ConnectionError):
        if client is not None:
            client.close()
        _set_cooldown()
        return None
    client.attach()  # busy late-join recovery
    with _CLIENT_LOCK:
        _CLIENT = client
    return client


def _set_cooldown() -> None:
    global _CLIENT_COOLDOWN_UNTIL
    with _CLIENT_LOCK:
        _CLIENT_COOLDOWN_UNTIL = time.monotonic() + _CONNECT_COOLDOWN


def connect() -> bool:
    """Eagerly bring hive's client online (spawn time / sidecar request)."""
    return _shared_client() is not None


def drop_client() -> None:
    """Close the process's client so the next use reconnects (daemon respawn)."""
    global _CLIENT, _CLIENT_COOLDOWN_UNTIL
    with _CLIENT_LOCK:
        client, _CLIENT = _CLIENT, None
        _CLIENT_COOLDOWN_UNTIL = 0.0
    if client is not None:
        client.close()


# --------------------------------------------------------------------------
# pane-keyed API (thread resolved through the pane's record)
# --------------------------------------------------------------------------
def runtime_for_pane(pane: str) -> ThreadRuntime | None:
    tid = thread_id_for_pane(pane)
    if not tid:
        return None
    client = _shared_client()
    if client is None:
        return None
    return client.runtime_or_backfill(tid)


def send_to_pane(pane: str, text: str) -> str | None:
    """Deliver text as a new turn on the pane's recorded thread.

    Returns ``TURN_START_ACCEPTED`` when ``turn/start`` answered with a
    result — the daemon accepted the turn, which is codex's transport
    boundary (not proof the turn ran to completion). Returns ``None`` on
    transport failure: no recorded thread (unmanaged codex), no daemon, an
    RPC error response, or a connection failure. There is no keystroke
    fallback — normal hive delivery never touches the composer. A *busy*
    thread is not bounced: ``turn/start`` carries steer semantics in core
    (it steers the text into the running turn, or opens a fresh turn when
    idle), so hive hands it straight to the RPC and lets codex pick the
    landing — the same thing the codex TUI does for a typed message.
    """
    tid = thread_id_for_pane(pane)
    if not tid:
        return None
    return send_to_thread(tid, text)


def send_to_thread(thread_id: str, text: str) -> str | None:
    """Deliver text as a new turn on *thread_id* — the engine-keyed core.

    Same transport contract as :func:`send_to_pane`; a pane-less member is
    addressed by the thread id its registry row carries.
    """
    client = _shared_client()
    if client is None:
        return None
    try:
        response = client.turn_start(thread_id, text)
    except Exception:  # noqa: BLE001 — RPC/socket failure is a transport failure
        return None
    return TURN_START_ACCEPTED if "result" in response else None


def interrupt_pane(pane: str) -> str | None:
    """Abort the running turn on the pane's recorded thread.

    Returns ``TURN_INTERRUPT_ACCEPTED`` when the daemon took the interrupt,
    ``NO_RUNNING_TURN`` when the thread has no in-progress turn (nothing to
    abort — not a failure), and ``None`` on transport failure: no recorded
    thread, no daemon, an RPC error, or a connection failure. There is no
    keystroke fallback: an Escape into the pane would land on whatever the
    viewer is showing, while ``turn/interrupt`` is addressed to the thread.
    """
    tid = thread_id_for_pane(pane)
    if not tid:
        return None
    return interrupt_thread(tid)


def interrupt_thread(thread_id: str) -> str | None:
    """Abort the running turn on *thread_id* — the engine-keyed core."""
    client = _shared_client()
    if client is None:
        return None
    try:
        turn_id = client.active_turn_id(thread_id)
        if not turn_id:
            return NO_RUNNING_TURN
        response = client.turn_interrupt(thread_id, turn_id)
    except Exception:  # noqa: BLE001 — RPC/socket failure is a transport failure
        return None
    return TURN_INTERRUPT_ACCEPTED if "result" in response else None


def compact_pane(pane: str) -> str:
    """Start context compaction on the pane's recorded thread.

    Compaction is *not* steerable: codex runs it as a Compact turn via
    ``spawn_task``, whose first act is to abort any running turn. Firing it
    at a busy agent would kill the in-flight work. So unlike a normal
    message, hive gates compaction on busy and only compacts an idle thread.

    Returns ``"compacted"`` (RPC accepted), ``"busy"`` (agent mid-turn), or
    ``"unavailable"`` (no record / no daemon). On anything but ``"compacted"``
    the caller keystrokes ``/compact`` into the TUI so codex itself surfaces
    its native "disabled while a task is in progress" refusal.
    """
    tid = thread_id_for_pane(pane)
    if not tid:
        return "unavailable"
    client = _shared_client()
    if client is None:
        return "unavailable"
    rt = client.runtime_or_backfill(tid)
    if rt is not None and rt.busy:
        return "busy"
    return "compacted" if "result" in client.compact_start(tid) else "unavailable"


def session_id_for_pane(pane: str) -> str | None:
    """Transcript session id of the pane's recorded thread.

    threadId == sessionId on the app-server surface, so this is a plain
    record read — no daemon round-trip and no lsof.
    """
    return thread_id_for_pane(pane)


# --------------------------------------------------------------------------
# spawn-flow helpers
# --------------------------------------------------------------------------
def freshen_models_cache() -> bool:
    """Renew ~/.codex/models_cache.json's fetched_at so a mint stays warm.

    thread/start synchronously refetches /models when the cache is older
    than codex's 300s TTL (~2.5s, up to its 5s timeout). The data barely
    changes and codex itself renews the stamp without refetching on an
    etag match, so extending the last real fetch is the same semantic;
    the daemon's periodic Online refresh still overwrites with real data.
    """
    path = codex_home() / "models_cache.json"
    try:
        entry = json.loads(path.read_text(encoding="utf-8"))
        entry["fetched_at"] = (
            time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()) + ".000000Z"
        )
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(entry), encoding="utf-8")
        os.replace(tmp, path)
        return True
    except (OSError, ValueError):
        return False


def start_member_thread(cwd: str, *, name: str, model: str = "") -> str | None:
    """Mint a resumable thread for a new member; None on any failure."""
    client = _shared_client()
    if client is None:
        return None
    freshen_models_cache()
    return client.start_thread(cwd, name=name, model=model)


def fork_member_thread(thread_id: str, *, name: str) -> str | None:
    """Server-side fork of *thread_id*; returns the fork's id, None on failure."""
    client = _shared_client()
    if client is None:
        return None
    freshen_models_cache()
    return client.fork_thread(thread_id, name=name)
