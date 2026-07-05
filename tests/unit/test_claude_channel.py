"""Unit tests for the Claude channel delivery adapter + stdio MCP server.

Covers the VAL contract: socket path derivation, ready-gated send_to_pane with
exact-byte preservation, prepare_pane writing a static --mcp-config under
$HIVE_HOME (never a project file), and the pure-Python MCP server's
initialize/notification framing plus server-owned ready-marker lifecycle.
"""
from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import pytest

from hive.adapters import claude_channel as cc

pytestmark = pytest.mark.unit


@pytest.fixture(autouse=True)
def _hive_home(monkeypatch):
    # A short base is required: AF_UNIX sun_path caps at ~104 bytes on macOS, so
    # the per-pane socket cannot live under pytest's long tmp_path. Production
    # HIVE_HOME (~/.hive) is short for the same reason the socket lives there.
    base = "/tmp" if os.path.isdir("/tmp") else tempfile.gettempdir()
    home = Path(tempfile.mkdtemp(prefix="hh", dir=base))
    monkeypatch.setenv("HIVE_HOME", str(home))
    yield home
    shutil.rmtree(home, ignore_errors=True)


# --- paths / readiness ------------------------------------------------------

def test_channel_socket_path_derives_from_hive_home_and_pane(_hive_home):
    p = cc.channel_socket_path("%108")
    assert p == _hive_home / "channel" / "hive-pane-108.sock"


def test_ready_marker_lifecycle():
    assert cc.is_ready("%5") is False
    marker = cc.ready_marker_path("%5")
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text("1")
    assert cc.is_ready("%5") is True
    cc.clear_ready("%5")
    assert cc.is_ready("%5") is False


def test_marker_paths_agree_between_adapter_and_server():
    # the server derives the marker from its socket path; both sides must
    # resolve the identical file or readiness gating silently breaks
    from hive.adapters import claude_channel_server as srv
    pane = "%42"
    assert srv.marker_path_for_socket(srv.socket_path_for_pane(pane)) == str(
        cc.ready_marker_path(pane)
    )


def _make_ready(pane: str) -> None:
    marker = cc.ready_marker_path(pane)
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text("1")


# --- send_to_pane -----------------------------------------------------------

def test_send_to_pane_false_without_ready_marker():
    assert cc.send_to_pane("%900", "msg") is False


def test_send_to_pane_false_when_ready_but_no_socket():
    _make_ready("%901")
    assert cc.send_to_pane("%901", "msg") is False


def test_send_to_pane_writes_exact_envelope_and_returns_true():
    pane = "%108"
    _make_ready(pane)
    sock_path = cc.channel_socket_path(pane)
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(sock_path))
    srv.listen(1)
    received: dict[str, bytes] = {}

    def _accept() -> None:
        conn, _ = srv.accept()
        chunks = []
        while True:
            buf = conn.recv(4096)
            if not buf:
                break
            chunks.append(buf)
        received["raw"] = b"".join(chunks)
        conn.close()

    threading.Thread(target=_accept, daemon=True).start()
    envelope = "<HIVE from=w to=v msgId=abc1>\nbody `tick` $(x)\nline3\n</HIVE>"
    try:
        assert cc.send_to_pane(pane, envelope) is True
        time.sleep(0.3)
    finally:
        srv.close()
    frame = json.loads(received["raw"].decode())
    assert frame["content"] == envelope  # multi-line + shell metachars intact
    assert frame["msg_id"] == "abc1"


def test_send_to_pane_false_on_refused_connect():
    # ready marker + a socket *path* that exists as a file but nothing listening
    pane = "%902"
    _make_ready(pane)
    sock_path = cc.channel_socket_path(pane)
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    sock_path.write_text("")  # not a live socket
    assert cc.send_to_pane(pane, "msg") is False


# --- prepare_pane -----------------------------------------------------------

def test_prepare_pane_writes_static_config_and_returns_flags(_hive_home, tmp_path):
    flags = cc.prepare_pane(str(tmp_path))

    cfg_path = _hive_home / "channel" / "mcp-config.json"
    assert flags == [
        "--dangerously-load-development-channels", "server:hive-channel",
        "--mcp-config", str(cfg_path),
    ]
    # --mcp-config is last so a following `--flag` terminates the variadic
    # list; a positional prompt still needs a `--` separator (spawn adds it).
    assert flags[-2] == "--mcp-config"

    cfg = json.loads(cfg_path.read_text())
    entry = cfg["mcpServers"]["hive-channel"]
    assert entry["type"] == "stdio"
    assert entry["command"] == sys.executable
    assert entry["args"] == ["-m", "hive.adapters.claude_channel_server"]
    assert entry["env"]["HIVE_HOME"] == str(_hive_home)
    assert "PYTHONPATH" in entry["env"]


def test_prepare_pane_never_touches_the_project_directory(tmp_path):
    before = set(os.listdir(tmp_path))
    cc.prepare_pane(str(tmp_path))
    assert set(os.listdir(tmp_path)) == before  # no .mcp.json, nothing


def test_prepare_pane_is_idempotent(_hive_home, tmp_path):
    first = cc.prepare_pane(str(tmp_path))
    second = cc.prepare_pane(str(tmp_path))
    assert first == second


def test_prepare_pane_returns_empty_when_config_unwritable(_hive_home, tmp_path):
    # occupy the channel dir path with a plain file so mkdir/write fails
    (_hive_home / "channel").write_text("not a directory")
    assert cc.prepare_pane(str(tmp_path)) == []


# --- stdio MCP server (no Claude) -------------------------------------------

def _server_proc(hive_home: Path, pane: str) -> subprocess.Popen:
    env = {**os.environ, "HIVE_HOME": str(hive_home), "TMUX_PANE": pane,
           "PYTHONPATH": str(Path(__file__).resolve().parents[2] / "src")}
    return subprocess.Popen(
        [sys.executable, "-m", "hive.adapters.claude_channel_server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1, env=env,
    )


def _wait_path(path: Path, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            return True
        time.sleep(0.05)
    return False


def test_server_initialize_notification_framing_and_marker(_hive_home):
    proc = _server_proc(_hive_home, "%77")
    try:
        proc.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }) + "\n")
        proc.stdin.flush()
        resp = json.loads(proc.stdout.readline())
        assert resp["id"] == 0
        assert resp["result"]["capabilities"]["experimental"]["claude/channel"] == {}
        assert resp["result"]["serverInfo"]["name"] == "hive-channel"

        sock_path = _hive_home / "channel" / "hive-pane-77.sock"
        marker = _hive_home / "channel" / "hive-pane-77.ready"
        assert _wait_path(sock_path)  # derived from TMUX_PANE
        assert _wait_path(marker)  # server owns readiness: bound => marker
        assert stat.S_IMODE(sock_path.stat().st_mode) == 0o600
        assert stat.S_IMODE(sock_path.parent.stat().st_mode) == 0o700

        conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        conn.connect(str(sock_path))
        conn.sendall(json.dumps({"msg_id": "z9", "content": "<HIVE>x\ny</HIVE>"}).encode())
        conn.shutdown(socket.SHUT_WR)
        conn.close()
        note = json.loads(proc.stdout.readline())
        assert note["method"] == "notifications/claude/channel"
        assert note["params"]["content"] == "<HIVE>x\ny</HIVE>"
        assert note["params"]["meta"] == {"msg_id": "z9"}
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_server_sigterm_removes_socket_and_marker(_hive_home):
    proc = _server_proc(_hive_home, "%78")
    sock_path = _hive_home / "channel" / "hive-pane-78.sock"
    marker = _hive_home / "channel" / "hive-pane-78.ready"
    try:
        assert _wait_path(sock_path) and _wait_path(marker)
        proc.send_signal(signal.SIGTERM)
        proc.wait(timeout=5)
        deadline = time.time() + 3
        while time.time() < deadline and (sock_path.exists() or marker.exists()):
            time.sleep(0.05)
        assert not sock_path.exists()
        assert not marker.exists()
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=5)


def test_server_bind_failure_writes_no_marker(_hive_home):
    # occupy the socket path with a directory: the pre-bind unlink fails and
    # bind raises, so the server must not report readiness
    sock_path = _hive_home / "channel" / "hive-pane-79.sock"
    sock_path.mkdir(parents=True)
    marker = _hive_home / "channel" / "hive-pane-79.ready"
    proc = _server_proc(_hive_home, "%79")
    try:
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1,
                                     "method": "initialize", "params": {}}) + "\n")
        proc.stdin.flush()
        resp = json.loads(proc.stdout.readline())
        assert resp["id"] == 1  # handshake still works
        time.sleep(0.5)  # give the socket thread time to fail the bind
        assert not marker.exists()
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_server_without_tmux_pane_skips_socket(_hive_home):
    env = {**os.environ, "HIVE_HOME": str(_hive_home),
           "PYTHONPATH": str(Path(__file__).resolve().parents[2] / "src")}
    env.pop("TMUX_PANE", None)
    proc = subprocess.Popen(
        [sys.executable, "-m", "hive.adapters.claude_channel_server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1, env=env,
    )
    try:
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                     "params": {}}) + "\n")
        proc.stdin.flush()
        resp = json.loads(proc.stdout.readline())
        assert resp["id"] == 1  # handshake still works
        assert not (_hive_home / "channel").exists()  # no socket dir created
    finally:
        proc.terminate()
        proc.wait(timeout=5)
