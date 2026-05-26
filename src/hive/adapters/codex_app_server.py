"""Codex app-server client over a *per-pane* daemon.

Each hive-spawned codex pane runs its own ``codex app-server --listen
unix://<sock>`` daemon that shares the real CODEX_HOME. The daemon is started
with ``TMUX_PANE=<pane>`` in its environment, so shell tools it spawns inherit
the correct pane identity — codex copies the daemon process env into tool
subprocesses (``inherit:All``) with no per-thread TMUX_PANE injection. The codex
TUI in that pane connects with ``codex --remote unix://<sock> --cd <cwd>``;
hive connects as a second client over the same socket for runtime signals and
turn delivery.

Why per-pane and not one shared daemon: a single shared daemon freezes one
TMUX_PANE for every thread, so untagged codex shells silently impersonate the
first pane that spawned the daemon. One daemon per pane keeps identity honest
and lets the whole tmux-tag workaround go away.

Why ``--remote`` is safe here (codex 0.133.0 source-confirmed): ``--remote``
puts the TUI in ``Remote`` workspace mode, whose ``thread/start`` keeps model /
approval_policy / sandbox / cwd (cwd via ``--cd``) and only drops
``model_provider`` — which the shared real-CODEX_HOME server config supplies by
default. So workspace semantics stay intact.

Transport is WebSocket framing over the unix socket — verified against codex
0.133.0, the daemon answers an HTTP Upgrade with ``101 Switching Protocols``.
The client is stdlib-only (RFC6455 masked text frames), no new dependency. One
background reader thread per connection keeps a thread-keyed state store current.

busy late-join boundary (smoke-verified): a client online when a thread is
created receives the full active->idle broadcast. A late-joining client must
``thread/resume`` to recover the current state of an *active* thread; resuming
an *idle, not-yet-rolled-out* thread fails with ``no rollout found`` and is
harmless (retried). So hive attaches per pane and stays connected rather than
connecting lazily on each read.
"""

from __future__ import annotations

import base64
import json
import os
import signal
import socket
import struct
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path

_HANDSHAKE_TIMEOUT = 5.0
_CALL_TIMEOUT = 10.0
_DAEMON_START_TIMEOUT = 8.0
_RUNTIME_STALE_AFTER = 30.0
_CONNECT_COOLDOWN = 5.0
_RESUME_COOLDOWN = 5.0


def codex_home() -> Path:
    return Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))


def pane_socket_path(pane: str) -> Path:
    """Per-pane daemon socket under the real CODEX_HOME.

    Lives under ``app-server-control/`` (a real directory codex itself uses, so
    it is never a symlink — codex rejects a symlinked socket parent, e.g.
    ``/tmp`` on macOS). One socket per tmux pane id keeps daemons isolated; pane
    ids are unique within a tmux server.
    """
    slug = pane.replace("%", "") or "default"
    return codex_home() / "app-server-control" / f"hive-pane-{slug}.sock"


def pane_pidfile_path(pane: str) -> Path:
    """Sibling pidfile of the pane's daemon socket.

    Written when the daemon becomes ready so the sidecar (which does not start
    the daemon) can find and reap it when the pane dies.
    """
    return pane_socket_path(pane).with_suffix(".pid")


def _pane_from_socket_name(name: str) -> str | None:
    """Inverse of :func:`pane_socket_path`: ``hive-pane-19.sock`` -> ``%19``."""
    if not name.startswith("hive-pane-") or not name.endswith(".sock"):
        return None
    slug = name[len("hive-pane-"):-len(".sock")]
    if not slug or slug == "default":
        return None
    return "%" + slug


# --------------------------------------------------------------------------
# transport: minimal RFC6455 client over a unix socket (text frames, masked)
# --------------------------------------------------------------------------
class _WSConn:
    def __init__(self, path: str, timeout: float = _HANDSHAKE_TIMEOUT):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self._rx = b""
        self._handshake()

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
    active_turn_id: str | None = None
    tokens: int | None = None
    window: int | None = None
    observed_at: float = 0.0

    def is_fresh(self, now: float | None = None) -> bool:
        return (now or time.time()) - self.observed_at < _RUNTIME_STALE_AFTER


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
# one connection to one per-pane daemon
# --------------------------------------------------------------------------
class CodexDaemonClient:
    def __init__(self, socket_path: str):
        self.socket_path = socket_path
        self._conn = _WSConn(socket_path)
        self._send_lock = threading.Lock()
        self._state_lock = threading.Lock()
        self._pending: dict[int, dict] = {}
        self._threads: dict[str, ThreadRuntime] = {}
        self._session_ids: dict[str, str] = {}
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
            if rid is not None and rid in self._pending:
                slot = self._pending.pop(rid)
                slot["msg"] = msg
                slot["event"].set()
            elif msg.get("method"):
                self._on_notification(msg["method"], msg.get("params") or {})
        self._closed = True

    # ---- notification -> state ----
    def _on_notification(self, method: str, params: dict) -> None:
        tid = params.get("threadId")
        if not tid:
            return
        with self._state_lock:
            rt = self._threads.setdefault(tid, ThreadRuntime())
            rt.observed_at = time.time()
            if method == "thread/status/changed":
                _apply_status(rt, params.get("status") or {})
            elif method == "turn/started":
                turn = params.get("turn") or {}
                rt.active_turn_id = turn.get("id")
                rt.busy = True
                rt.turn_phase = "tool_open"
            elif method == "turn/completed":
                rt.active_turn_id = None
                rt.busy = False
                rt.turn_phase = "turn_closed"
                if not rt.input_state:
                    rt.input_state = "ready"
            elif method == "thread/tokenUsage/updated":
                usage = params.get("tokenUsage") or {}
                # `last` is the current context size (the most recent turn);
                # `total` is the cumulative sum across all turns and routinely
                # exceeds the context window, so it must NOT be used here.
                last = usage.get("last") or {}
                rt.tokens = last.get("totalTokens")
                if usage.get("modelContextWindow") is not None:
                    rt.window = usage.get("modelContextWindow")

    def runtime_for(self, thread_id: str) -> ThreadRuntime | None:
        with self._state_lock:
            rt = self._threads.get(thread_id)
            return ThreadRuntime(**rt.__dict__) if rt is not None else None

    def latest_runtime(self) -> ThreadRuntime | None:
        """Most-recently-observed thread's runtime.

        A per-pane daemon normally hosts a single live thread (that pane's codex
        session), so the pane's runtime is whichever thread last produced an
        event. Returns None before any event is seen.
        """
        with self._state_lock:
            if not self._threads:
                return None
            tid = max(self._threads, key=lambda t: self._threads[t].observed_at)
            return ThreadRuntime(**self._threads[tid].__dict__)

    def latest_thread_id(self) -> str | None:
        """Thread id of the most-recently-observed thread (for turn delivery)."""
        with self._state_lock:
            if not self._threads:
                return None
            return max(self._threads, key=lambda t: self._threads[t].observed_at)

    def ensure_session_id(self) -> str | None:
        """Transcript session id of the latest thread, from app-server metadata.

        Reads ``Thread.sessionId`` via ``thread/resume`` (cached). Resuming an
        idle, not-yet-rolled-out thread fails harmlessly, so the id only becomes
        available once the thread has activity — fine, since hive only needs it
        for resume/transcript links, which matter only after activity. Rate-
        limited per thread so an unresolved id does not storm resumes.
        """
        tid = self.latest_thread_id()
        if not tid:
            return None
        with self._state_lock:
            sid = self._session_ids.get(tid)
            if sid:
                return sid
            if time.monotonic() < self._resume_cooldown.get(tid, 0.0):
                return None
            self._resume_cooldown[tid] = time.monotonic() + _RESUME_COOLDOWN
        self.resume(tid)  # fills _session_ids on success
        with self._state_lock:
            return self._session_ids.get(tid)

    # ---- protocol helpers ----
    def initialize(self) -> bool:
        res = self.call("initialize", {
            "clientInfo": {"name": "hive", "title": "hive", "version": "1"},
            "capabilities": {"experimentalApi": True},
        })
        return "result" in res

    def attach(self) -> None:
        """Recover state for already-active threads (busy late-join).

        A client online at thread creation gets the full broadcast; this covers
        the late-join case by resuming each loaded thread once. Resuming an
        idle, not-yet-rolled-out thread fails with `no rollout found` — harmless.
        """
        for tid in self.loaded_list():
            self.resume(tid)

    def thread_list(self, cwd: str) -> list[dict]:
        res = self.call("thread/list", {"cwd": cwd})
        return (res.get("result") or {}).get("data") or [] if "result" in res else []

    def loaded_list(self) -> list[str]:
        res = self.call("thread/loaded/list", {})
        return (res.get("result") or {}).get("data") or [] if "result" in res else []

    def resume(self, thread_id: str) -> bool:
        res = self.call("thread/resume", {"threadId": thread_id, "excludeTurns": True})
        result = res.get("result")
        if not isinstance(result, dict):
            return False
        # thread/resume returns the full Thread, whose sessionId is the transcript
        # session id (the rollout file's UUID). Cache it: this is the reliable
        # source, vs lsof which the daemon does not always expose.
        thread = result.get("thread")
        if isinstance(thread, dict):
            sid = thread.get("sessionId")
            if sid:
                with self._state_lock:
                    self._session_ids[thread_id] = str(sid)
        return True

    def turn_start(self, thread_id: str, text: str) -> dict:
        return self.call("turn/start", {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
        })

    def interrupt(self, thread_id: str, turn_id: str) -> dict:
        return self.call("turn/interrupt", {"threadId": thread_id, "turnId": turn_id})

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


def spawn_daemon(
    pane: str,
    *,
    codex_bin: str = "codex",
    timeout: float = _DAEMON_START_TIMEOUT,
) -> bool:
    """Ensure a per-pane app-server daemon is listening; return True if ready.

    Reuses a live daemon if one already answers on the pane's socket (idempotent
    spawn). Otherwise starts one, injecting ``TMUX_PANE=<pane>`` so shell tools
    report the right pane and sharing the real CODEX_HOME (auth/model/permission
    defaults stay correct). ``start_new_session`` detaches it from the
    short-lived CLI; the sidecar reaps it via the pidfile when the pane dies.
    Returns False if the daemon fails to bind or dies before becoming ready.
    """
    sock = pane_socket_path(pane)
    sock.parent.mkdir(parents=True, exist_ok=True)
    if sock.exists():
        if probe_socket(str(sock)):
            return True  # reuse the live daemon
        try:
            sock.unlink()  # stale socket from a dead daemon
        except OSError:
            pass
    env = dict(os.environ)
    env["TMUX_PANE"] = pane
    try:
        proc = subprocess.Popen(
            [codex_bin, "app-server", "--listen", f"unix://{sock}"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
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
                pane_pidfile_path(pane).write_text(str(proc.pid))
            except OSError:
                pass
            return True
        time.sleep(0.2)
    try:
        proc.terminate()
    except OSError:
        pass
    return False


def list_daemon_panes() -> list[str]:
    """Pane ids that currently have a per-pane daemon socket on disk."""
    root = codex_home() / "app-server-control"
    if not root.is_dir():
        return []
    panes: list[str] = []
    for entry in root.glob("hive-pane-*.sock"):
        pane = _pane_from_socket_name(entry.name)
        if pane:
            panes.append(pane)
    return panes


def _terminate_process_group(pid: int) -> None:
    """SIGTERM the pid's process group, escalating to SIGKILL if it lingers.

    spawn_daemon uses ``start_new_session``, so the daemon is a process-group
    leader and its app-server child shares the group. ``killpg`` reaps both; a
    plain ``kill(pid)`` on the node wrapper would orphan the Rust child.
    """
    try:
        pgid = os.getpgid(pid)
    except (OSError, ProcessLookupError):
        return
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (OSError, ProcessLookupError):
            return
        for _ in range(10):  # up to ~1s before escalating
            try:
                os.kill(pid, 0)
            except (OSError, ProcessLookupError):
                return  # exited
            time.sleep(0.1)


def kill_pane_daemon(pane: str) -> None:
    """Stop a pane's daemon and remove its socket + pidfile (best-effort)."""
    pidfile = pane_pidfile_path(pane)
    try:
        pid: int | None = int(pidfile.read_text().strip())
    except (OSError, ValueError):
        pid = None
    if pid is not None:
        _terminate_process_group(pid)
    for path in (pane_socket_path(pane), pidfile):
        try:
            path.unlink()
        except OSError:
            pass


def _session_id_via_daemon_lsof(pane: str) -> str | None:
    """Fallback: transcript session id via lsof on the daemon pid.

    Unreliable — the daemon does not always hold the rollout JSONL open (codex
    real-machine smoke saw empty FILES during an active turn) — so this is only
    a fallback behind :func:`session_id_for_pane`'s app-server lookup.
    """
    try:
        pid = int(pane_pidfile_path(pane).read_text().strip())
    except (OSError, ValueError):
        return None
    from .. import tmux
    from .codex import session_id_from_open_file

    for fpath in tmux.list_open_files(pid):
        sid = session_id_from_open_file(fpath)
        if sid:
            return sid
    return None


# --------------------------------------------------------------------------
# per-pane connection pool (sidecar-side)
# --------------------------------------------------------------------------
class CodexClientPool:
    """One persistent app-server connection per pane.

    The sidecar calls :meth:`runtime_for_pane` every tick; the per-connection
    reader thread keeps each pane's thread state current between calls.
    Connections are lazily established the first time a read finds a live daemon
    socket and then reused; a dead connection is dropped and retried after a
    short cooldown so a missing daemon does not storm reconnects.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._clients: dict[str, CodexDaemonClient] = {}
        self._cooldown: dict[str, float] = {}

    def runtime_for_pane(self, pane: str) -> ThreadRuntime | None:
        client = self._client_for(pane)
        return client.latest_runtime() if client is not None else None

    def connect(self, pane: str) -> bool:
        """Eagerly bring the 2nd client online for a pane.

        Called at spawn time — after the daemon is up but before codex creates
        its thread — so the client is already connected when ``thread/started``
        and the per-turn ``tokenUsage`` broadcasts fire. Runtime is then tracked
        live from the broadcast stream; no late-join resume is needed.
        """
        return self._client_for(pane) is not None

    def send_to_pane(self, pane: str, text: str) -> bool:
        """Deliver text as a new turn over the pane's daemon.

        Returns False (caller should fall back to keystroke injection) when
        there is no daemon, no thread yet, or a turn is already active — codex
        turn/start *steers* into a running turn rather than queuing after it, so
        an active thread is delivered through the composer instead.
        """
        client = self._client_for(pane)
        if client is None:
            return False
        tid = client.latest_thread_id()
        if not tid:
            return False
        rt = client.runtime_for(tid)
        if rt is not None and rt.busy:
            return False
        return "result" in client.turn_start(tid, text)

    def session_id_for_pane(self, pane: str) -> str | None:
        client = self._client_for(pane)
        return client.ensure_session_id() if client is not None else None

    def _client_for(self, pane: str) -> CodexDaemonClient | None:
        with self._lock:
            client = self._clients.get(pane)
            if client is not None and client.is_alive():
                return client
            if client is not None:
                client.close()
                self._clients.pop(pane, None)
            if time.monotonic() < self._cooldown.get(pane, 0.0):
                return None

        sock = pane_socket_path(pane)
        if not sock.exists():
            self._set_cooldown(pane)
            return None
        new_client: CodexDaemonClient | None = None
        try:
            new_client = CodexDaemonClient(str(sock))
            if not new_client.initialize():
                raise ConnectionError("initialize failed")
        except (OSError, ConnectionError):
            if new_client is not None:
                new_client.close()
            self._set_cooldown(pane)
            return None
        new_client.attach()  # busy late-join recovery
        with self._lock:
            self._clients[pane] = new_client
        return new_client

    def _set_cooldown(self, pane: str) -> None:
        with self._lock:
            self._cooldown[pane] = time.monotonic() + _CONNECT_COOLDOWN

    def drop(self, pane: str) -> None:
        with self._lock:
            client = self._clients.pop(pane, None)
        if client is not None:
            client.close()


_POOL: CodexClientPool | None = None
_POOL_LOCK = threading.Lock()


def pool() -> CodexClientPool:
    global _POOL
    with _POOL_LOCK:
        if _POOL is None:
            _POOL = CodexClientPool()
        return _POOL


def runtime_for_pane(pane: str) -> ThreadRuntime | None:
    return pool().runtime_for_pane(pane)


def connect_pane(pane: str) -> bool:
    return pool().connect(pane)


def send_to_pane(pane: str, text: str) -> bool:
    return pool().send_to_pane(pane, text)


def session_id_for_pane(pane: str) -> str | None:
    """Transcript session id: app-server thread metadata first, lsof fallback."""
    sid = pool().session_id_for_pane(pane)
    return sid if sid else _session_id_via_daemon_lsof(pane)
