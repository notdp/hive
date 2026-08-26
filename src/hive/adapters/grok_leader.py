"""Grok leader client over a *per-pane* leader daemon.

Each hive-spawned grok pane runs its own ``grok agent leader --leader-socket
<sock>`` daemon sharing the real GROK_HOME. The grok TUI in that pane attaches
to it (``grok --leader --leader-socket <sock> --session-id <uuid>``); hive
attaches as a second client through ``grok agent --leader stdio --leader-socket
<sock>`` — a subprocess speaking ACP JSON-RPC 2.0 as newline-delimited JSON on
stdin/stdout. The leader's own socket protocol is private, so hive never talks
to the socket directly: the stdio subprocess is the supported door.

Which session that second client drives is not discoverable from the leader
(``session/list`` returns every session of the cwd), so hive mints the pane's
session id at spawn time and records it beside the socket in a ``.session``
file. The client loads exactly that session and folds only its notifications.

``session/load`` replays the session's past ``session/update`` notifications
before it answers, so everything received before the load response is dropped —
a replayed turn must never mark the pane busy. Delivery acks on the leader
echoing the prompt back (queue entry or ``user_message_chunk``): the
``session/prompt`` response itself only lands when the whole turn ends, which
can be minutes.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import threading
import time
import weakref
from dataclasses import dataclass
from pathlib import Path

_INIT_TIMEOUT = 10.0   # initialize answers ~2 s after process start
_LOAD_TIMEOUT = 5.0    # session/load ~0.8 s plus the notification replay
_HANDSHAKE_TIMEOUT = _INIT_TIMEOUT + _LOAD_TIMEOUT
_ACK_TIMEOUT = 10.0
_CALL_TIMEOUT = 10.0
_DAEMON_START_TIMEOUT = 8.0
_CONNECT_COOLDOWN = 5.0

# Worst-case local submission budget for one send_to_pane call: a cold client
# (initialize + session/load) plus the ack wait. The sidecar derives its request
# budgets from this so a valid slow acceptance can never outlive its caller.
SUBMIT_TIMEOUT = _HANDSHAKE_TIMEOUT + _ACK_TIMEOUT

# Accepted-transport classification for durable delivery observations: the
# leader took the prompt into the session queue. Not proof the turn ran.
PROMPT_QUEUED = "sessionPromptQueued"

_TOOL_PHASES = ("tool_open", "tool_result_pending_reply")
_MESSAGE_CHUNKS = ("agent_message_chunk", "agent_thought_chunk", "user_message_chunk")


def grok_home() -> Path:
    return Path(os.environ.get("GROK_HOME", str(Path.home() / ".grok")))


def pane_socket_path(pane: str) -> Path:
    """Per-pane leader socket under the real GROK_HOME.

    Deliberately short (``hive/p19.sock``): AF_UNIX paths cap at 104 bytes and
    the leader binds this path itself. One socket per tmux pane id keeps
    daemons isolated; pane ids are unique within a tmux server.
    """
    slug = pane.replace("%", "") or "default"
    return grok_home() / "hive" / f"p{slug}.sock"


def pane_pidfile_path(pane: str) -> Path:
    """Sibling pidfile of the pane's leader socket.

    Written once the socket appears so the sidecar (which does not start the
    daemon) can prove liveness and reap it when the pane dies.
    """
    return pane_socket_path(pane).with_suffix(".pid")


def pane_session_path(pane: str) -> Path:
    """Sibling record of the session id hive minted for this pane."""
    return pane_socket_path(pane).with_suffix(".session")


def write_pane_session(pane: str, session_id: str, cwd: str) -> None:
    path = pane_session_path(pane)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"sessionId": session_id, "cwd": cwd}))


def read_pane_session(pane: str) -> tuple[str, str] | None:
    try:
        data = json.loads(pane_session_path(pane).read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    session_id, cwd = data.get("sessionId"), data.get("cwd")
    if not session_id or not cwd:
        return None
    return str(session_id), str(cwd)


def _daemon_env_for_pane(pane: str) -> dict[str, str]:
    """Leader env: the member pane's identity, nothing inherited that lies.

    The spawner may itself run inside another member's engine (an orch's
    flow runner), whose env carries that engine's identity markers —
    CLAUDE_CODE_MESSAGING_SOCKET would make every hive call inside this
    grok member resolve to the *orch's* pane. Wash them; pin our own.
    """
    env = {
        k: v for k, v in os.environ.items()
        if not (k.startswith("CLAUDE") or k.startswith("ANTHROPIC") or k == "CODEX_THREAD_ID")
    }
    env["TMUX_PANE"] = pane
    return env


def _pane_from_socket_name(name: str) -> str | None:
    """Inverse of :func:`pane_socket_path`: ``p19.sock`` -> ``%19``."""
    if not name.startswith("p") or not name.endswith(".sock"):
        return None
    slug = name[1:-len(".sock")]
    return "%" + slug if slug.isdigit() else None


# --------------------------------------------------------------------------
# per-session runtime state, kept current by the reader thread
# --------------------------------------------------------------------------
@dataclass
class SessionRuntime:
    busy: bool = False
    turn_phase: str = "unknown_evidence"
    input_state: str = ""
    session_id: str | None = None
    observed_at: float = 0.0


# --------------------------------------------------------------------------
# one stdio client attached to one pane's leader
# --------------------------------------------------------------------------
class GrokStdioClient:
    """``grok agent --leader stdio`` subprocess bound to one pane's session."""

    def __init__(self, pane: str):
        self.pane = pane
        self.socket_path = str(pane_socket_path(pane))
        self._proc = subprocess.Popen(
            ["grok", "agent", "--leader", "stdio", "--leader-socket", self.socket_path],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        # A short-lived CLI (`hive compact`) exits without close(); without this
        # the stdio child outlives it and holds a leader connection forever.
        weakref.finalize(self, self._proc.terminate)
        self._io_lock = threading.Lock()
        self._state_lock = threading.Lock()
        self._pending: dict[int, dict] = {}
        self._runtime = SessionRuntime()
        self._ack: dict | None = None
        self._loaded = False
        self._id = 0
        self._closed = False
        self._reader = threading.Thread(target=self._reader_loop, daemon=True)
        self._reader.start()

    # ---- request/response ----
    def _next_id(self) -> int:
        with self._io_lock:
            self._id += 1
            return self._id

    def _write(self, message: dict) -> bool:
        with self._io_lock:
            try:
                self._proc.stdin.write(json.dumps(message) + "\n")
                self._proc.stdin.flush()
                return True
            except (OSError, ValueError):
                return False

    def call(
        self,
        method: str,
        params: dict | None = None,
        timeout: float = _CALL_TIMEOUT,
        loads: str | None = None,
    ) -> dict:
        """One request/response. ``loads`` marks the call that binds a session.

        The reader thread flips the client to loaded before waking this waiter,
        so a notification queued right behind the response is folded instead of
        being mistaken for replay.
        """
        if self._closed:
            return {"__error__": "closed"}
        rid = self._next_id()
        slot: dict = {"event": threading.Event(), "msg": None}
        if loads is not None:
            slot["loads"] = loads
        self._pending[rid] = slot
        if not self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}):
            self._pending.pop(rid, None)
            return {"__error__": "write failed"}
        if not slot["event"].wait(timeout):
            self._pending.pop(rid, None)
            return {"__timeout__": True}
        msg = slot["msg"] or {}
        if "error" in msg:
            return {"__error__": msg["error"]}
        return {"result": msg.get("result")}

    def _reader_loop(self) -> None:
        stdout = self._proc.stdout
        while not self._closed:
            try:
                line = stdout.readline()
            except (OSError, ValueError):
                break
            if not line:
                break  # process death
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if not isinstance(msg, dict):
                continue
            method, rid = msg.get("method"), msg.get("id")
            if method and rid is not None:
                self._on_request(rid, method, msg.get("params") or {})
            elif method:
                self._on_notification(method, msg.get("params") or {})
            else:
                # Pop atomically: a `call()` that timed out concurrently may have
                # removed this rid already, and a missing slot only means the
                # waiter is gone — drop the late response instead of raising.
                slot = self._pending.pop(rid, None) if rid is not None else None
                if slot is not None:
                    if "loads" in slot and "error" not in msg:
                        with self._state_lock:
                            self._runtime.session_id = slot["loads"]
                            self._loaded = True
                    slot["msg"] = msg
                    slot["event"].set()
        self._closed = True
        self._fail_pending()

    def _fail_pending(self) -> None:
        """Fail every in-flight waiter: a dead child never answers them."""
        while self._pending:
            _rid, slot = self._pending.popitem()
            slot["msg"] = {"error": "closed"}
            slot["event"].set()

    # ---- agent -> client request ----
    def _on_request(self, rid: object, method: str, params: dict) -> None:
        """Answer a permission prompt with ``cancelled``.

        The decision belongs to the human at the TUI, which gets its own copy of
        the request; hive must still answer its copy or the turn stalls, and
        cancelling is the only answer that neither approves nor rejects for them.
        """
        if method != "session/request_permission":
            return
        self._write({"jsonrpc": "2.0", "id": rid, "result": {"outcome": {"outcome": "cancelled"}}})
        with self._state_lock:
            if params.get("sessionId") != self._runtime.session_id:
                return
            self._runtime.input_state = "waiting_user"
            self._runtime.observed_at = time.time()

    # ---- notification -> state ----
    def _on_notification(self, method: str, params: dict) -> None:
        with self._state_lock:
            if not self._loaded:
                return  # session/load replays past updates; replay is not evidence
            rt = self._runtime
            if method == "_x.ai/sessions/changed":
                for entry in params.get("upserted") or []:
                    if isinstance(entry, dict) and entry.get("sessionId") == rt.session_id:
                        self._apply_activity(entry.get("activity"))
                return
            if params.get("sessionId") != rt.session_id:
                return
            rt.observed_at = time.time()
            if method == "session/update":
                self._apply_update(params.get("update") or {})
            elif method == "_x.ai/session_notification":
                if (params.get("update") or {}).get("sessionUpdate") == "turn_completed":
                    rt.busy = False
                    rt.turn_phase = "turn_closed"
                    rt.input_state = "ready"
            elif method == "_x.ai/queue/changed":
                self._apply_queue(params)

    def _apply_activity(self, activity: object) -> None:
        """Fold ``activity`` — the leader's busy authority — into the runtime."""
        rt = self._runtime
        rt.observed_at = time.time()
        if activity == "working":
            rt.busy = True
        elif activity == "idle":
            rt.busy = False
            rt.turn_phase = "turn_closed"
            rt.input_state = "ready"

    def _apply_update(self, update: dict) -> None:
        rt = self._runtime
        kind = update.get("sessionUpdate")
        if kind == "tool_call":
            rt.busy = True
            rt.turn_phase = "tool_open"
        elif kind == "tool_call_update":
            # An update on a tool call means the turn is running and any
            # permission it was blocked on has been decided.
            rt.busy = True
            rt.input_state = "ready"
            if update.get("status") == "completed":
                rt.turn_phase = "tool_result_pending_reply"
        elif kind in _MESSAGE_CHUNKS:
            rt.busy = True
            if rt.turn_phase not in _TOOL_PHASES:
                rt.turn_phase = "user_prompt_pending"
            if kind == "user_message_chunk":
                self._note_ack((update.get("content") or {}).get("text"))

    def _apply_queue(self, params: dict) -> None:
        entries = [e for e in params.get("entries") or [] if isinstance(e, dict)]
        if entries:
            self._runtime.turn_phase = "input_backlog"
        for text in [e.get("text") for e in entries] + [params.get("runningText")]:
            self._note_ack(text)

    def _note_ack(self, text: object) -> None:
        ack = self._ack
        if ack is not None and text is not None and text == ack["text"]:
            ack["event"].set()

    # ---- protocol ----
    def handshake(self) -> bool:
        """``initialize`` then ``session/load`` of the pane's minted session.

        Both values come from the pane session file — the pane's cwd is recorded
        at spawn time, so no tmux query is needed here.
        """
        session = read_pane_session(self.pane)
        if session is None:
            return False
        initialized = self.call("initialize", {
            "protocolVersion": 1,
            "clientInfo": {"name": "hive", "version": "1"},
            "clientCapabilities": {},
        }, timeout=_INIT_TIMEOUT)
        if "result" not in initialized:
            return False
        session_id, cwd = session
        loaded = self.call("session/load", {
            "sessionId": session_id,
            "cwd": cwd,
            "mcpServers": [],
        }, timeout=_LOAD_TIMEOUT, loads=session_id)
        return "result" in loaded

    def prompt(self, text: str) -> bool:
        """Queue one prompt; True once the leader echoes it back.

        The ``session/prompt`` response only arrives when the turn ends, so the
        accept boundary is the echo — a queue entry carrying the text, or the
        turn's ``user_message_chunk``. The response id stays registered just long
        enough to catch an immediate rpc error; its eventual result is dropped.
        """
        done = threading.Event()
        with self._state_lock:
            session_id = self._runtime.session_id
            self._ack = {"text": text, "event": done}
        rid = self._next_id()
        slot = {"event": done, "msg": None}
        self._pending[rid] = slot
        try:
            sent = self._write({"jsonrpc": "2.0", "id": rid, "method": "session/prompt", "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}],
            }})
            if not sent or not done.wait(_ACK_TIMEOUT):
                return False
            msg = slot["msg"]
            return not (isinstance(msg, dict) and "error" in msg)
        finally:
            self._pending.pop(rid, None)
            with self._state_lock:
                self._ack = None

    def compact(self) -> str:
        """Compact the session's context; ``busy`` defers instead of aborting.

        Compaction replaces the running turn, so a mid-turn agent is left alone
        and the caller keystrokes ``/compact`` into the TUI instead.
        """
        with self._state_lock:
            busy = self._runtime.busy
            session_id = self._runtime.session_id
        if busy:
            return "busy"
        result = self.call("x.ai/compact_conversation", {"sessionId": session_id})
        return "compacted" if "result" in result else "unavailable"

    def runtime(self) -> SessionRuntime | None:
        """Snapshot, or None while nothing has been observed for this session."""
        with self._state_lock:
            rt = self._runtime
            return SessionRuntime(**rt.__dict__) if rt.observed_at else None

    @property
    def session_id(self) -> str | None:
        """Session this client is bound to, so the pool can spot a rotation."""
        with self._state_lock:
            return self._runtime.session_id

    def is_alive(self) -> bool:
        return not self._closed and self._reader.is_alive() and self._proc.poll() is None

    def close(self) -> None:
        self._closed = True
        self._fail_pending()
        try:
            self._proc.stdin.close()
        except OSError:
            pass
        try:
            self._proc.terminate()
            self._proc.wait(timeout=1.0)
        except (OSError, subprocess.TimeoutExpired):
            pass


# --------------------------------------------------------------------------
# daemon lifecycle
# --------------------------------------------------------------------------
def probe_socket(socket_path: str) -> bool:
    """True when the socket exists and its recorded daemon pid is alive.

    No ACP traffic: the leader's socket protocol is private, so liveness is the
    pidfile plus ``kill(pid, 0)`` rather than a handshake.
    """
    path = Path(socket_path)
    if not path.exists():
        return False
    try:
        pid = int(path.with_suffix(".pid").read_text().strip())
        os.kill(pid, 0)
    except (OSError, ValueError):
        return False
    return True


def spawn_daemon(pane: str, *, grok_bin: str = "grok", timeout: float = _DAEMON_START_TIMEOUT) -> bool:
    """Ensure a per-pane leader daemon is listening; return True if ready.

    Idempotent: a live daemon on the pane's socket is reused. Otherwise one is
    started with ``TMUX_PANE=<pane>`` so its shell tools report the right pane,
    sharing the real GROK_HOME (auth/model/session layout stay correct).
    ``start_new_session`` detaches it from the short-lived CLI; the sidecar reaps
    it through the pidfile when the pane dies.
    """
    sock = pane_socket_path(pane)
    sock.parent.mkdir(parents=True, exist_ok=True)
    if sock.exists():
        if probe_socket(str(sock)):
            return True
        try:
            sock.unlink()  # stale socket from a dead daemon
        except OSError:
            pass
    try:
        proc = subprocess.Popen(
            [
                grok_bin, "agent", "leader",
                "--leader-socket", str(sock),
                "--no-auto-update",
                "--no-exit-on-disconnect",
            ],
            env=_daemon_env_for_pane(pane),
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
        if sock.exists():
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
    """Pane ids that currently have a per-pane leader socket on disk."""
    root = grok_home() / "hive"
    if not root.is_dir():
        return []
    panes = [_pane_from_socket_name(entry.name) for entry in root.glob("p*.sock")]
    return [pane for pane in panes if pane]


def _terminate_process_group(pid: int) -> None:
    """SIGTERM the pid's process group, escalating to SIGKILL if it lingers.

    spawn_daemon uses ``start_new_session``, so the leader is a process-group
    leader and its children share the group; ``killpg`` reaps them together.
    """
    try:
        pgid = os.getpgid(pid)
    except OSError:
        return
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except OSError:
            return
        for _ in range(10):  # up to ~1s before escalating
            try:
                os.kill(pid, 0)
            except OSError:
                return  # exited
            time.sleep(0.1)


def kill_pane_daemon(pane: str) -> None:
    """Stop a pane's leader and remove its socket, pidfile and session record."""
    pidfile = pane_pidfile_path(pane)
    try:
        pid: int | None = int(pidfile.read_text().strip())
    except (OSError, ValueError):
        pid = None
    if pid is not None:
        _terminate_process_group(pid)
    sock = pane_socket_path(pane)
    for path in (sock, sock.with_suffix(".lock"), pidfile, pane_session_path(pane)):
        try:
            path.unlink()
        except OSError:
            pass


# --------------------------------------------------------------------------
# per-pane client pool (sidecar-side)
# --------------------------------------------------------------------------
class GrokClientPool:
    """One persistent stdio client per pane.

    The sidecar calls :meth:`runtime_for_pane` every tick; each client's reader
    thread keeps its session state current between calls. Clients are created
    lazily the first time a read finds both a socket and a session record, and a
    dead one is dropped and retried after a cooldown so a missing daemon does not
    storm subprocess spawns.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._clients: dict[str, GrokStdioClient] = {}
        self._cooldown: dict[str, float] = {}

    def runtime_for_pane(self, pane: str) -> SessionRuntime | None:
        client = self._client_for(pane)
        return client.runtime() if client is not None else None

    def connect(self, pane: str) -> bool:
        """Bring the stdio client online for a pane (called at spawn time)."""
        return self._client_for(pane) is not None

    def send_to_pane(self, pane: str, text: str) -> str | None:
        """Deliver text as a prompt over the pane's leader.

        Returns ``PROMPT_QUEUED`` when the leader echoed the prompt back, else
        None: no daemon, no session record, an rpc error, or an ack timeout.
        A busy session is not bounced — the leader queues the prompt FIFO and
        runs it when the current turn ends, the same as typing into the TUI.
        """
        client = self._client_for(pane)
        if client is None:
            return None
        try:
            return PROMPT_QUEUED if client.prompt(text) else None
        except Exception:  # noqa: BLE001 — any client failure is a transport failure
            return None

    def compact_pane(self, pane: str) -> str:
        client = self._client_for(pane)
        return client.compact() if client is not None else "unavailable"

    def _client_for(self, pane: str) -> GrokStdioClient | None:
        # A relaunched grok in the same pane mints a new session id, so the
        # record — not just the client's liveness — decides whether the bound
        # client is still the pane's.
        record = read_pane_session(pane)
        with self._lock:
            client = self._clients.get(pane)
            if client is not None:
                if client.is_alive() and record is not None and client.session_id == record[0]:
                    return client
                client.close()
                self._clients.pop(pane, None)
            if time.monotonic() < self._cooldown.get(pane, 0.0):
                return None

        if record is None or not probe_socket(str(pane_socket_path(pane))):
            self._set_cooldown(pane)
            return None
        new_client: GrokStdioClient | None = None
        try:
            new_client = GrokStdioClient(pane)
            if not new_client.handshake():
                raise ConnectionError("handshake failed")
        except (OSError, ValueError, ConnectionError):
            if new_client is not None:
                new_client.close()
            self._set_cooldown(pane)
            return None
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


_POOL: GrokClientPool | None = None
_POOL_LOCK = threading.Lock()


def pool() -> GrokClientPool:
    global _POOL
    with _POOL_LOCK:
        if _POOL is None:
            _POOL = GrokClientPool()
        return _POOL


def runtime_for_pane(pane: str) -> SessionRuntime | None:
    return pool().runtime_for_pane(pane)


def connect_pane(pane: str) -> bool:
    return pool().connect(pane)


def send_to_pane(pane: str, text: str) -> str | None:
    return pool().send_to_pane(pane, text)


def compact_pane(pane: str) -> str:
    return pool().compact_pane(pane)


def session_id_for_pane(pane: str) -> str | None:
    """Session id hive minted for this pane, from its session record."""
    session = read_pane_session(pane)
    return session[0] if session else None
