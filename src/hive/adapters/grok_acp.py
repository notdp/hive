"""Per-pane Grok leader + ACP client.

Each hive-spawned grok pane gets its own ``grok agent leader`` on a socket
under ``$HIVE_HOME/grok/``. The TUI is launched with ``--leader-socket``
pointing at that socket. Hive is a second client.

Identity is the leader pid + session id in :mod:`hive.identity`, not a
forged ``TMUX_PANE``.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path

from .. import identity

_HANDSHAKE_TIMEOUT = 5.0
_CALL_TIMEOUT = 10.0
SUBMIT_TIMEOUT = _HANDSHAKE_TIMEOUT + _CALL_TIMEOUT
_DAEMON_START_TIMEOUT = 8.0
_CONNECT_COOLDOWN = 5.0

PROMPT_ACCEPTED = "sessionPromptAccepted"


def _hive_home() -> Path:
    return Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))


def pane_socket_path(pane: str) -> Path:
    slug = pane.replace("%", "") or "default"
    return _hive_home() / "grok" / f"hive-pane-{slug}.sock"


def pane_pidfile_path(pane: str) -> Path:
    return pane_socket_path(pane).with_suffix(".pid")


def _pane_from_socket_name(name: str) -> str | None:
    if not name.startswith("hive-pane-") or not name.endswith(".sock"):
        return None
    slug = name[len("hive-pane-"):-len(".sock")]
    if not slug or slug == "default":
        return None
    return "%" + slug


@dataclass
class SessionRuntime:
    busy: bool = False
    turn_phase: str = "unknown_evidence"
    input_state: str = ""
    session_id: str | None = None
    observed_at: float = 0.0


class _AcpConn:
    """JSON-RPC over a unix socket: NDJSON first, then Codex-style websocket."""

    def __init__(self, path: str, timeout: float = _HANDSHAKE_TIMEOUT):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self._rx = b""
        self.mode = "ndjson"

    def send_msg(self, msg: dict) -> None:
        payload = (json.dumps(msg) + "\n").encode()
        self.sock.sendall(payload)

    def recv_msg(self) -> dict:
        while b"\n" not in self._rx:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("grok leader closed")
            self._rx += chunk
        line, self._rx = self._rx.split(b"\n", 1)
        if not line.strip():
            return self.recv_msg()
        parsed = json.loads(line.decode("utf-8", "replace"))
        if not isinstance(parsed, dict):
            raise ConnectionError("non-object from grok leader")
        return parsed

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


class GrokLeaderClient:
    def __init__(self, socket_path: str, pane: str = ""):
        self.socket_path = socket_path
        self.pane = pane
        self._conn = _AcpConn(socket_path)
        self._send_lock = threading.Lock()
        self._state_lock = threading.Lock()
        self._pending: dict[int, dict] = {}
        self._runtime = SessionRuntime()
        self._id = 0
        self._closed = False
        self._reader = threading.Thread(target=self._reader_loop, daemon=True)
        self._reader.start()

    def call(self, method: str, params: dict | None = None, timeout: float = _CALL_TIMEOUT) -> dict:
        with self._send_lock:
            if self._closed:
                return {"__error__": "closed"}
            self._id += 1
            rid = self._id
            slot = {"event": threading.Event(), "msg": None}
            self._pending[rid] = slot
            body = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}
            try:
                self._conn.send_msg(body)
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
                msg = self._conn.recv_msg()
            except (OSError, ConnectionError, ValueError):
                break
            rid = msg.get("id")
            slot = self._pending.pop(rid, None) if rid is not None else None
            if slot is not None:
                slot["msg"] = msg
                slot["event"].set()
            elif msg.get("method"):
                self._on_notification(msg["method"], msg.get("params") or {})
        self._closed = True

    def _on_notification(self, method: str, params: dict) -> None:
        update = params.get("update") if isinstance(params, dict) else None
        if not isinstance(update, dict):
            return
        kind = update.get("sessionUpdate")
        session_id = params.get("sessionId") if isinstance(params, dict) else None
        with self._state_lock:
            self._runtime.observed_at = time.time()
            if isinstance(session_id, str) and session_id:
                self._runtime.session_id = session_id
                if self.pane:
                    identity.bind(identity.Binding(
                        pane_id=self.pane,
                        cli="grok",
                        session_id=session_id,
                    ))
            if kind in {"tool_call", "agent_message_chunk", "agent_thought_chunk", "user_message_chunk"}:
                self._runtime.busy = True
                self._runtime.turn_phase = "tool_open" if kind == "tool_call" else "user_prompt_pending"
                if kind == "tool_call":
                    tool = ((update.get("_meta") or {}).get("x.ai/tool") or {})
                    name = str(tool.get("name") or update.get("title") or "")
                    if name in {"ask_user_question", "AskUserQuestion", "request_user_input"}:
                        self._runtime.input_state = "waiting_user"
                    else:
                        self._runtime.input_state = "ready"
            elif kind == "turn_completed":
                self._runtime.busy = False
                self._runtime.turn_phase = "turn_closed"
                self._runtime.input_state = "ready"

    def initialize(self) -> bool:
        res = self.call("initialize", {
            "protocolVersion": 1,
            "clientInfo": {"name": "hive", "title": "hive", "version": "1"},
            "clientCapabilities": {},
        })
        return "result" in res

    def ensure_session(self, cwd: str) -> str | None:
        with self._state_lock:
            if self._runtime.session_id:
                return self._runtime.session_id
        loaded = self.call("session/load", {})
        result = loaded.get("result")
        if isinstance(result, dict):
            sid = result.get("sessionId") or (result.get("session") or {}).get("sessionId")
            if sid:
                self._set_session(str(sid))
                return str(sid)
        created = self.call("session/new", {"cwd": cwd, "mcpServers": []})
        result = created.get("result")
        if isinstance(result, dict):
            sid = result.get("sessionId")
            if sid:
                self._set_session(str(sid))
                return str(sid)
        with self._state_lock:
            return self._runtime.session_id

    def _set_session(self, session_id: str) -> None:
        with self._state_lock:
            self._runtime.session_id = session_id
            self._runtime.observed_at = time.time()
        if self.pane:
            identity.bind(identity.Binding(pane_id=self.pane, cli="grok", session_id=session_id))

    def prompt(self, text: str, cwd: str) -> bool:
        sid = self.ensure_session(cwd)
        if not sid:
            return False
        res = self.call("session/prompt", {
            "sessionId": sid,
            "prompt": [{"type": "text", "text": text}],
        })
        return "result" in res

    def runtime(self) -> SessionRuntime | None:
        with self._state_lock:
            if self._runtime.observed_at == 0 and not self._runtime.session_id:
                return None
            return SessionRuntime(**self._runtime.__dict__)

    def is_alive(self) -> bool:
        return not self._closed and self._reader.is_alive()

    def close(self) -> None:
        self._closed = True
        self._conn.close()


def probe_socket(socket_path: str) -> bool:
    try:
        conn = _AcpConn(socket_path, timeout=2.0)
    except OSError:
        return False
    try:
        conn.send_msg({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "hive-probe", "version": "0"}},
        })
        msg = conn.recv_msg()
        return msg.get("id") == 1
    except (OSError, ConnectionError, ValueError):
        return False
    finally:
        conn.close()


def spawn_daemon(pane: str, *, grok_bin: str = "grok", timeout: float = _DAEMON_START_TIMEOUT) -> bool:
    sock = pane_socket_path(pane)
    sock.parent.mkdir(parents=True, exist_ok=True)
    if sock.exists():
        if probe_socket(str(sock)) or sock.exists():
            # live socket: prefer reuse even if ACP probe fails (leader may
            # speak a private framing; the TUI can still attach)
            if pane_pidfile_path(pane).exists():
                return True
        try:
            sock.unlink()
        except OSError:
            pass
    env = dict(os.environ)
    try:
        proc = subprocess.Popen(
            [
                grok_bin, "agent", "leader",
                "--leader-socket", str(sock),
                "--no-exit-on-disconnect",
            ],
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
            return False
        if sock.exists():
            try:
                pane_pidfile_path(pane).write_text(str(proc.pid))
            except OSError:
                pass
            identity.bind(identity.Binding(pane_id=pane, cli="grok", pid=proc.pid))
            return True
        time.sleep(0.2)
    try:
        proc.terminate()
    except OSError:
        pass
    return False


def list_daemon_panes() -> list[str]:
    root = _hive_home() / "grok"
    if not root.is_dir():
        return []
    panes: list[str] = []
    for entry in root.glob("hive-pane-*.sock"):
        pane = _pane_from_socket_name(entry.name)
        if pane:
            panes.append(pane)
    return panes


def _terminate_process_group(pid: int) -> None:
    try:
        pgid = os.getpgid(pid)
    except (OSError, ProcessLookupError):
        return
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (OSError, ProcessLookupError):
            return
        for _ in range(10):
            try:
                os.kill(pid, 0)
            except (OSError, ProcessLookupError):
                return
            time.sleep(0.1)


def kill_pane_daemon(pane: str) -> None:
    pidfile = pane_pidfile_path(pane)
    try:
        pid: int | None = int(pidfile.read_text().strip())
    except (OSError, ValueError):
        pid = None
    if pid is not None:
        _terminate_process_group(pid)
        identity.drop_pid(pid)
    for path in (pane_socket_path(pane), pidfile):
        try:
            path.unlink()
        except OSError:
            pass


class GrokClientPool:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._clients: dict[str, GrokLeaderClient] = {}
        self._cooldown: dict[str, float] = {}

    def runtime_for_pane(self, pane: str) -> SessionRuntime | None:
        client = self._client_for(pane)
        return client.runtime() if client is not None else None

    def connect(self, pane: str) -> bool:
        return self._client_for(pane) is not None

    def send_to_pane(self, pane: str, text: str, cwd: str = "") -> str | None:
        client = self._client_for(pane)
        if client is None:
            return None
        try:
            ok = client.prompt(text, cwd or os.getcwd())
        except Exception:
            return None
        return PROMPT_ACCEPTED if ok else None

    def session_id_for_pane(self, pane: str) -> str | None:
        client = self._client_for(pane)
        if client is None:
            bound = identity.lookup_pid(_pid_from_pane(pane) or -1)
            return bound.session_id if bound and bound.session_id else None
        rt = client.runtime()
        if rt and rt.session_id:
            return rt.session_id
        return client.ensure_session(os.getcwd())

    def _client_for(self, pane: str) -> GrokLeaderClient | None:
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
        new_client: GrokLeaderClient | None = None
        try:
            new_client = GrokLeaderClient(str(sock), pane=pane)
            if not new_client.initialize():
                # leader may not speak ACP; keep the process, drop the client
                new_client.close()
                self._set_cooldown(pane)
                return None
        except (OSError, ConnectionError):
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


def _pid_from_pane(pane: str) -> int | None:
    try:
        return int(pane_pidfile_path(pane).read_text().strip())
    except (OSError, ValueError):
        return None


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


def send_to_pane(pane: str, text: str, cwd: str = "") -> str | None:
    return pool().send_to_pane(pane, text, cwd=cwd)


def session_id_for_pane(pane: str) -> str | None:
    return pool().session_id_for_pane(pane)
