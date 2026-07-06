"""Pure-stdlib stdio MCP "channel" server for Claude Code.

Claude spawns this as an MCP stdio subprocess (registered via
``claude_channel.prepare_pane``). It declares the ``claude/channel`` capability
so Claude registers a notification listener, then pushes each inbound line as a
``notifications/claude/channel`` event -- the programmatic replacement for tmux
send-keys delivery into a running Claude session.

Inbound seam: a per-pane unix socket under ``$HIVE_HOME/channel`` whose name is
derived from ``$TMUX_PANE`` (so the plugin's single server entry serves every
pane). ``claude_channel.send_to_pane`` connects and writes one JSON frame
``{"msg_id": ..., "content": ...}``; this server emits it to Claude. One way:
replies still go out through the ``hive`` CLI.

This server also owns the pane's ready marker: written once the socket is
listening, removed on exit. A live marker only proves this server is up and
bound; Claude delivering the notifications additionally requires the machine's
managed-settings channels allowlist, which ``claude_channel.prepare_pane``
preflights before any launch.
"""
from __future__ import annotations

import atexit
import json
import os
import signal
import socket
import sys
import threading

SERVER_NAME = "hive-channel"
INSTRUCTIONS = (
    'Messages from the hive channel arrive as <channel source="hive-channel" '
    'msg_id="...">, wrapping a <HIVE ...> envelope. Read the inner <HIVE> block '
    "and follow the hive protocol exactly as if it were injected directly. "
    "This channel is one-way: reply with the hive CLI, not a channel tool."
)

_stdout_lock = threading.Lock()


def _emit(obj: dict) -> None:
    line = json.dumps(obj, separators=(",", ":"))
    with _stdout_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def _log(msg: str) -> None:
    sys.stderr.write(f"[hive-channel] {msg}\n")
    sys.stderr.flush()


def _hive_home() -> str:
    return os.environ.get("HIVE_HOME") or os.path.join(os.path.expanduser("~"), ".hive")


def socket_path_for_pane(pane: str) -> str:
    slug = pane.replace("%", "") or "default"
    return os.path.join(_hive_home(), "channel", f"hive-pane-{slug}.sock")


def marker_path_for_socket(sock_path: str) -> str:
    """Same name shape as ``claude_channel.ready_marker_path``."""
    return sock_path[: -len(".sock")] + ".ready"


def _resolve_socket_path() -> str | None:
    pane = os.environ.get("TMUX_PANE", "")
    return socket_path_for_pane(pane) if pane else None


def _safe_unlink(path: str) -> None:
    try:
        os.unlink(path)
    except OSError:
        pass


def _recv_frame(conn: socket.socket) -> bytes:
    chunks = []
    while True:
        buf = conn.recv(65536)
        if not buf:
            break
        chunks.append(buf)
    return b"".join(chunks)


def _socket_loop(path: str) -> None:
    directory = os.path.dirname(path)
    os.makedirs(directory, exist_ok=True)
    try:
        os.chmod(directory, 0o700)  # owner-only where supported
    except OSError:
        pass
    _safe_unlink(path)  # clear a stale socket from a dead server before bind
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        srv.bind(path)
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
        srv.listen(16)
    except OSError as e:
        _log(f"bind failed for {path}: {e}")
        return  # no marker: the pane stays not-ready on bind failure
    try:
        with open(marker_path_for_socket(path), "w") as fh:
            fh.write("1")
    except OSError as e:
        _log(f"cannot write ready marker: {e}")
    _log(f"listening on {path}")
    while True:
        try:
            conn, _ = srv.accept()
        except OSError:
            break
        with conn:
            raw = _recv_frame(conn)
        if not raw:
            continue
        try:
            frame = json.loads(raw.decode("utf-8"))
            content = frame["content"]
            msg_id = frame.get("msg_id") or ""
        except (ValueError, KeyError, UnicodeDecodeError):
            continue
        meta = {"msg_id": msg_id} if msg_id else {}
        _emit({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {"content": content, "meta": meta},
        })


def _handle_request(method: str, params: dict) -> dict:
    if method == "initialize":
        return {
            "protocolVersion": params.get("protocolVersion") or "2025-06-18",
            "capabilities": {"experimental": {"claude/channel": {}}},
            "serverInfo": {"name": SERVER_NAME, "version": "0.1.0"},
            "instructions": INSTRUCTIONS,
        }
    if method == "ping":
        return {}
    raise _MethodNotFound(method)


class _MethodNotFound(Exception):
    pass


def main() -> None:
    path = _resolve_socket_path()
    if path:
        # Signal handlers must be installed from the main thread; unlink the
        # socket + ready marker on SIGTERM/SIGINT (Claude killing the MCP
        # child) since atexit does not run on signal termination. A stale
        # socket is also cleared before bind on the next spawn, and spawn
        # clears a stale marker before launch, so this is best-effort.
        def _remove_artifacts() -> None:
            _safe_unlink(path)
            _safe_unlink(marker_path_for_socket(path))

        def _cleanup(*_args: object) -> None:
            _remove_artifacts()
            os._exit(0)

        try:
            signal.signal(signal.SIGTERM, _cleanup)
            signal.signal(signal.SIGINT, _cleanup)
        except (ValueError, OSError):
            pass
        atexit.register(_remove_artifacts)
        threading.Thread(target=_socket_loop, args=(path,), daemon=True).start()
    else:
        _log("no TMUX_PANE; channel socket disabled (MCP handshake only)")
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if "id" not in msg:  # notification (e.g. notifications/initialized)
            continue
        try:
            result = _handle_request(msg.get("method", ""), msg.get("params") or {})
            _emit({"jsonrpc": "2.0", "id": msg["id"], "result": result})
        except _MethodNotFound as e:
            _emit({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32601, "message": f"method not found: {e}"},
            })


if __name__ == "__main__":
    main()
