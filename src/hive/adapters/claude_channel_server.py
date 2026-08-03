"""Pure-stdlib stdio MCP "channel" server for Claude Code.

Claude spawns this as an MCP stdio subprocess (registered via
``claude_channel.prepare_pane``). It declares the ``claude/channel`` capability
so Claude registers a notification listener, then pushes each inbound line as a
``notifications/claude/channel`` event -- the programmatic replacement for tmux
send-keys delivery into a running Claude session.

Inbound seam: a per-pane unix socket under ``$HIVE_HOME/channel`` whose name is
derived from ``$TMUX_PANE`` (so the plugin's single server entry serves every
pane). Outside tmux (Claude Code desktop) the socket is pid-keyed
(``hive-client-<pid>.sock``) and its path is appended to the MCP instructions;
``hive duo init --channel`` symlinks an anchor pane's socket to it so
pane-addressed delivery reaches the session. ``claude_channel.send_to_pane``
connects and writes one JSON frame
``{"msg_id": ..., "content": ...}``; this server emits it to Claude and then
answers with a single-byte **local MCP-write receipt** (``b"1"``) on the same
connection. The receipt only proves the notification was written+flushed to
this process's MCP stdio transport — the boundary the Channels contract itself
defines (``mcp.notification()`` resolves on transport write). It is NOT a
Claude-processing or final-delivery acknowledgement: per the contract,
notifications are unacknowledged and an unloaded/policy-blocked channel drops
them silently. Replies still go out through the ``hive`` CLI (one way).

This server also owns the pane's ready marker: content ``2`` (receipt-capable),
written only after the MCP ``initialize`` response has itself been emitted and
flushed, removed on exit. Legacy servers wrote ``1`` (no receipt). A live
marker only proves this server is up and initialized; Claude delivering the
notifications additionally requires the machine's managed-settings channels
allowlist, which ``claude_channel.prepare_pane`` preflights before any launch.
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
# Appended for sessions outside tmux (Claude Code desktop): the agent reads its
# own socket path from these instructions and hands it to `hive duo init
# --channel` — the agent itself is the bridge between this MCP server and the
# hive CLI, so no separate discovery mechanism exists.
CLIENT_INSTRUCTIONS_SUFFIX = (
    " This session runs outside tmux; its channel socket is {path}. To form a "
    "duo from here, run: hive duo init --channel {path}"
)

_stdout_lock = threading.Lock()
# The ready marker is published only once BOTH gates are open: the socket is
# bound+listening AND the MCP initialize response has been emitted+flushed.
# A sender can therefore never observe "ready" before the MCP session exists,
# and a failed bind never advertises a dead socket. Frames arriving before
# initialize are dropped without a receipt (the sender reports failure).
_initialized = threading.Event()
_socket_ready = threading.Event()
_marker_once = threading.Lock()
_marker_published = False
MARKER_RECEIPT_CAPABLE = "2"


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


def socket_path_for_client(pid: int) -> str:
    """Socket for a Claude session outside tmux (e.g. Claude Code desktop).

    Keyed by this server's pid — unique per session, dies with it. An anchor
    pane's ``hive-pane-*.sock`` is symlinked here by ``hive duo init
    --channel`` so pane-addressed delivery reaches the external session.
    """
    return os.path.join(_hive_home(), "channel", f"hive-client-{pid}.sock")


def marker_path_for_socket(sock_path: str) -> str:
    """Same name shape as ``claude_channel.ready_marker_path``."""
    return sock_path[: -len(".sock")] + ".ready"


def _resolve_socket_path() -> str:
    pane = os.environ.get("TMUX_PANE", "")
    return socket_path_for_pane(pane) if pane else socket_path_for_client(os.getpid())


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
    _socket_ready.set()
    _maybe_publish_marker(path)
    _log(f"listening on {path} (marker published after MCP initialize)")
    while True:
        try:
            conn, _ = srv.accept()
        except OSError:
            break
        with conn:
            raw = _recv_frame(conn)
            if not raw:
                continue
            if not _initialized.is_set():
                continue  # pre-initialize frame: close without receipt
            try:
                frame = json.loads(raw.decode("utf-8"))
                content = frame["content"]
                msg_id = frame.get("msg_id") or ""
            except (ValueError, KeyError, UnicodeDecodeError):
                continue  # malformed frame: close without receipt
            meta = {"msg_id": msg_id} if msg_id else {}
            try:
                _emit({
                    "jsonrpc": "2.0",
                    "method": "notifications/claude/channel",
                    "params": {"content": content, "meta": meta},
                })
            except Exception as e:  # noqa: BLE001 — MCP stdio gone: no receipt
                _log(f"MCP emit failed: {e}")
                continue
            try:
                # Local MCP-write receipt: the notification is on the MCP
                # stdio transport. A legacy client that already closed its
                # read side raises here — ignore and keep serving.
                conn.sendall(b"1")
            except OSError:
                pass


def _maybe_publish_marker(path: str) -> None:
    """Publish the readiness marker atomically once both gates are open.

    The visible marker path must never expose empty/partial content (a
    concurrent ``marker_version()`` would fail closed), so the content is
    written to a same-directory temp file and ``os.replace``d into place.
    The published flag is only set after a successful replace: a failed
    publish stays retryable on the next gate event.
    """
    global _marker_published
    if not (_initialized.is_set() and _socket_ready.is_set()):
        return
    with _marker_once:
        if _marker_published:
            return
        marker = marker_path_for_socket(path)
        tmp = marker + ".tmp"
        try:
            with open(tmp, "w") as fh:
                fh.write(MARKER_RECEIPT_CAPABLE)
            os.replace(tmp, marker)
        except OSError as e:
            _safe_unlink(tmp)
            _log(f"cannot write ready marker: {e}")
            return
        _marker_published = True


def _handle_request(method: str, params: dict) -> dict:
    if method == "initialize":
        instructions = INSTRUCTIONS
        if not os.environ.get("TMUX_PANE", ""):
            path = _resolve_socket_path()
            instructions += CLIENT_INSTRUCTIONS_SUFFIX.format(path=path)
        return {
            "protocolVersion": params.get("protocolVersion") or "2025-06-18",
            "capabilities": {"experimental": {"claude/channel": {}}},
            "serverInfo": {"name": SERVER_NAME, "version": "0.1.0"},
            "instructions": instructions,
        }
    if method == "ping":
        return {}
    raise _MethodNotFound(method)


class _MethodNotFound(Exception):
    pass


def main() -> None:
    path = _resolve_socket_path()
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
            # Readiness strictly follows the initialize RESPONSE reaching the
            # MCP transport — never the request handling alone. Only now may a
            # sender observe the pane as channel-ready.
            if msg.get("method") == "initialize" and not _initialized.is_set():
                _initialized.set()
                _maybe_publish_marker(path)
        except _MethodNotFound as e:
            _emit({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32601, "message": f"method not found: {e}"},
            })


if __name__ == "__main__":
    main()
