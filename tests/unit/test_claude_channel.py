"""Unit tests for the Claude channel delivery adapter + stdio MCP server.

Covers the VAL contract: socket path derivation, ready-gated send_to_pane with
exact-byte preservation, prepare_pane registration that stays invisible to git
(and refuses to touch a tracked .mcp.json), git-root resolution from a subdir,
and the pure-Python MCP server's initialize/notification framing without Claude.
"""
from __future__ import annotations

import json
import os
import shutil
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


def _git_init(path: Path) -> None:
    subprocess.run(["git", "-C", str(path), "init", "-q"], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.email", "t@t"], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.name", "t"], check=True)


# --- paths / readiness ------------------------------------------------------

def test_channel_socket_path_derives_from_hive_home_and_pane(_hive_home):
    p = cc.channel_socket_path("%108")
    assert p == _hive_home / "channel" / "hive-pane-108.sock"


def test_ready_marker_lifecycle():
    assert cc.is_ready("%5") is False
    cc.mark_ready("%5")
    assert cc.is_ready("%5") is True
    cc.clear_ready("%5")
    assert cc.is_ready("%5") is False


# --- send_to_pane -----------------------------------------------------------

def test_send_to_pane_false_without_ready_marker():
    assert cc.send_to_pane("%900", "msg") is False


def test_send_to_pane_false_when_ready_but_no_socket():
    cc.mark_ready("%901")
    assert cc.send_to_pane("%901", "msg") is False


def test_send_to_pane_writes_exact_envelope_and_returns_true():
    pane = "%108"
    cc.mark_ready(pane)
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
    cc.mark_ready(pane)
    sock_path = cc.channel_socket_path(pane)
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    sock_path.write_text("")  # not a live socket
    assert cc.send_to_pane(pane, "msg") is False


# --- prepare_pane -----------------------------------------------------------

def test_prepare_pane_writes_invisible_mcp_json(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    flags = cc.prepare_pane(str(repo))

    assert flags == ["--dangerously-load-development-channels", "server:hive-channel"]
    assert "--mcp-config" not in flags
    assert "--strict-mcp-config" not in flags

    cfg = json.loads((repo / ".mcp.json").read_text())
    entry = cfg["mcpServers"]["hive-channel"]
    assert entry["type"] == "stdio"  # canonical Claude Code server shape
    assert entry["command"] == sys.executable
    assert entry["args"] == ["-m", "hive.adapters.claude_channel_server"]
    assert "PYTHONPATH" in entry["env"] and "HIVE_HOME" in entry["env"]

    # invisible to git: status clean, check-ignore confirms the exclude
    status = subprocess.run(["git", "-C", str(repo), "status", "--short"],
                            capture_output=True, text=True).stdout
    assert ".mcp.json" not in status
    ignored = subprocess.run(["git", "-C", str(repo), "check-ignore", ".mcp.json"],
                             capture_output=True, text=True)
    assert ignored.returncode == 0


def test_prepare_pane_resolves_git_root_from_subdir(tmp_path):
    repo = tmp_path / "repo"
    (repo / "sub" / "deep").mkdir(parents=True)
    _git_init(repo)
    flags = cc.prepare_pane(str(repo / "sub" / "deep"))
    assert flags  # channel enabled
    assert (repo / ".mcp.json").exists()  # at git root
    assert not (repo / "sub" / "deep" / ".mcp.json").exists()


def test_prepare_pane_merges_tracked_mcp_json_via_skip_worktree(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    (repo / ".mcp.json").write_text('{"mcpServers": {"other": {"command": "x"}}}')
    subprocess.run(["git", "-C", str(repo), "add", ".mcp.json"], check=True)
    subprocess.run(["git", "-C", str(repo), "commit", "-qm", "add"], check=True)

    flags = cc.prepare_pane(str(repo))
    assert flags  # channel registered even for a tracked config

    cfg = json.loads((repo / ".mcp.json").read_text())
    assert "other" in cfg["mcpServers"]  # user's server preserved
    assert "hive-channel" in cfg["mcpServers"]  # merged in

    # the local addition is hidden via skip-worktree: status clean + committed
    # blob unchanged, so it can never be staged/committed by accident
    status = subprocess.run(["git", "-C", str(repo), "status", "--short"],
                            capture_output=True, text=True).stdout
    assert ".mcp.json" not in status
    ls = subprocess.run(["git", "-C", str(repo), "ls-files", "-v", ".mcp.json"],
                        capture_output=True, text=True).stdout
    assert ls.startswith("S")  # skip-worktree bit set
    head = subprocess.run(["git", "-C", str(repo), "show", "HEAD:.mcp.json"],
                          capture_output=True, text=True).stdout
    assert "hive-channel" not in head  # committed version untouched


def test_release_pane_restores_tracked_mcp_json(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    committed = '{"mcpServers": {"other": {"command": "x"}}}'
    (repo / ".mcp.json").write_text(committed)
    subprocess.run(["git", "-C", str(repo), "add", ".mcp.json"], check=True)
    subprocess.run(["git", "-C", str(repo), "commit", "-qm", "add"], check=True)

    cc.prepare_pane(str(repo))
    cc.release_pane(str(repo))

    ls = subprocess.run(["git", "-C", str(repo), "ls-files", "-v", ".mcp.json"],
                        capture_output=True, text=True).stdout
    assert ls.startswith("H")  # skip-worktree cleared (tracked, normal)
    cfg = json.loads((repo / ".mcp.json").read_text())
    assert "hive-channel" not in cfg["mcpServers"]  # restored to committed


def test_prepare_pane_merges_existing_untracked_mcp_json(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    (repo / ".mcp.json").write_text('{"mcpServers": {"other": {"command": "x"}}}')
    flags = cc.prepare_pane(str(repo))
    assert flags
    cfg = json.loads((repo / ".mcp.json").read_text())
    assert "other" in cfg["mcpServers"]  # preserved
    assert "hive-channel" in cfg["mcpServers"]  # added


def test_prepare_pane_preserves_other_top_level_keys(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    (repo / ".mcp.json").write_text('{"someTool": {"k": 1}, "mcpServers": {"a": {}}}')
    cc.prepare_pane(str(repo))
    cfg = json.loads((repo / ".mcp.json").read_text())
    assert cfg["someTool"] == {"k": 1}  # unrelated top-level key kept
    assert "a" in cfg["mcpServers"] and "hive-channel" in cfg["mcpServers"]


def test_prepare_pane_overwrites_invalid_mcp_json(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    (repo / ".mcp.json").write_text("{not json, not a usable config")

    flags = cc.prepare_pane(str(repo))
    assert flags  # not a JSON object -> replaced, channel registered
    cfg = json.loads((repo / ".mcp.json").read_text())
    assert "hive-channel" in cfg["mcpServers"]


def test_prepare_pane_overwrites_non_object_mcp_json(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    (repo / ".mcp.json").write_text('["valid json", "but not an object"]')

    flags = cc.prepare_pane(str(repo))
    assert flags
    cfg = json.loads((repo / ".mcp.json").read_text())
    assert isinstance(cfg, dict) and "hive-channel" in cfg["mcpServers"]


def test_prepare_pane_replaces_non_object_servers_keeps_keys(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _git_init(repo)
    (repo / ".mcp.json").write_text('{"keep": 1, "mcpServers": "not-an-object"}')

    flags = cc.prepare_pane(str(repo))
    assert flags
    cfg = json.loads((repo / ".mcp.json").read_text())
    assert cfg["keep"] == 1  # other keys preserved
    assert cfg["mcpServers"] == {"hive-channel": cfg["mcpServers"]["hive-channel"]}


def test_prepare_pane_outside_git_uses_cwd(tmp_path):
    plain = tmp_path / "plain"
    plain.mkdir()
    flags = cc.prepare_pane(str(plain))
    assert flags
    assert (plain / ".mcp.json").exists()


# --- stdio MCP server (no Claude) -------------------------------------------

def _server_proc(hive_home: Path, pane: str) -> subprocess.Popen:
    env = {**os.environ, "HIVE_HOME": str(hive_home), "TMUX_PANE": pane,
           "PYTHONPATH": str(Path(__file__).resolve().parents[2] / "src")}
    return subprocess.Popen(
        [sys.executable, "-m", "hive.adapters.claude_channel_server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1, env=env,
    )


def test_server_initialize_and_notification_framing(_hive_home):
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
        for _ in range(100):
            if sock_path.exists():
                break
            time.sleep(0.05)
        assert sock_path.exists()  # derived from TMUX_PANE
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
