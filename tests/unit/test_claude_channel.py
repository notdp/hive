"""Unit tests for the Claude channel delivery adapter + stdio MCP server.

Covers the VAL contract: socket path derivation, ready-gated send_to_pane with
exact-byte preservation, prepare_pane converging the hive plugin marketplace
under $HIVE_HOME (never a project file) and returning plain --channels flags,
and the pure-Python MCP server's initialize/notification framing plus
server-owned ready-marker lifecycle.
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
def _allowlisted(monkeypatch, tmp_path):
    # tests must not depend on the host's real managed-settings policy;
    # this is the exact shape the setup hint writes (the verified one)
    policy = tmp_path / "managed-settings.json"
    policy.write_text(json.dumps({
        "channelsEnabled": True,
        "allowedChannelPlugins": [
            {"marketplace": "hive", "plugin": "hive-channel"}],
    }))
    monkeypatch.setattr(cc, "_MANAGED_SETTINGS_PATHS", (str(policy),))
    return policy


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


def _make_ready(pane: str, version: str = cc.MARKER_RECEIPT_CAPABLE) -> None:
    marker = cc.ready_marker_path(pane)
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text(version)


def _accept_server(sock_path: Path, *, receipt: bytes | None, received: dict) -> socket.socket:
    """One-shot fake channel server: read a frame to EOF, optionally answer."""
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(sock_path))
    srv.listen(1)

    def _serve() -> None:
        conn, _ = srv.accept()
        chunks = []
        while True:
            buf = conn.recv(4096)
            if not buf:
                break
            chunks.append(buf)
        received["raw"] = b"".join(chunks)
        if receipt is not None:
            try:
                conn.sendall(receipt)
            except OSError:
                pass
        conn.close()

    threading.Thread(target=_serve, daemon=True).start()
    return srv


# --- send_to_pane -----------------------------------------------------------

def test_send_to_pane_fails_without_ready_marker():
    assert cc.send_to_pane("%900", "msg") is None


def test_send_to_pane_fails_when_ready_but_no_socket():
    _make_ready("%901")
    assert cc.send_to_pane("%901", "msg") is None


@pytest.mark.parametrize("marker", ["", "3", "receipt"])
def test_send_to_pane_fails_closed_on_unknown_marker(marker):
    # An empty/corrupt/future marker must never be guessed as legacy.
    _make_ready("%903", version=marker)
    assert cc.send_to_pane("%903", "msg") is None


def test_send_to_pane_receipt_accepts_and_preserves_envelope():
    pane = "%108"
    _make_ready(pane)  # marker "2": receipt required
    received: dict[str, bytes] = {}
    srv = _accept_server(cc.channel_socket_path(pane), receipt=b"1", received=received)
    envelope = "<HIVE from=w to=v msgId=abc1>\nbody `tick` $(x)\nline3\n</HIVE>"
    try:
        assert cc.send_to_pane(pane, envelope) == cc.ACCEPTED_MCP_WRITE
        time.sleep(0.3)
    finally:
        srv.close()
    frame = json.loads(received["raw"].decode())
    assert frame["content"] == envelope
    assert frame["msg_id"] == "abc1"


def test_send_to_pane_legacy_marker_accepts_without_receipt():
    pane = "%109"
    _make_ready(pane, version=cc.MARKER_LEGACY)
    received: dict[str, bytes] = {}
    srv = _accept_server(cc.channel_socket_path(pane), receipt=None, received=received)
    try:
        # Old server never answers; the legacy path must not wait for a
        # receipt (and must never time out into a failure).
        assert cc.send_to_pane(pane, "<HIVE from=w to=v msgId=led1>x</HIVE>") == cc.ACCEPTED_LEGACY_SOCKET
        time.sleep(0.3)
    finally:
        srv.close()
    assert json.loads(received["raw"].decode())["msg_id"] == "led1"


def test_send_to_pane_fails_when_receipt_server_closes_without_receipt():
    # Race per VAL-1: the UNIX write succeeded but the server died before the
    # MCP emit — the frame must count as lost, never as accepted/queued.
    pane = "%110"
    _make_ready(pane)
    received: dict[str, bytes] = {}
    srv = _accept_server(cc.channel_socket_path(pane), receipt=None, received=received)
    try:
        assert cc.send_to_pane(pane, "<HIVE from=w to=v msgId=rce1>x</HIVE>") is None
    finally:
        srv.close()


def test_send_to_pane_fails_on_wrong_receipt_byte():
    pane = "%111"
    _make_ready(pane)
    received: dict[str, bytes] = {}
    srv = _accept_server(cc.channel_socket_path(pane), receipt=b"0", received=received)
    try:
        assert cc.send_to_pane(pane, "<HIVE from=w to=v msgId=wrb1>x</HIVE>") is None
    finally:
        srv.close()


def test_send_to_pane_fails_on_refused_connect():
    # ready marker + a socket *path* that exists as a file but nothing listening
    pane = "%902"
    _make_ready(pane)
    sock_path = cc.channel_socket_path(pane)
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    sock_path.write_text("")  # not a live socket
    assert cc.send_to_pane(pane, "msg") is None


# --- prepare_pane (published marketplace convergence) ------------------------

class _FakeClaudePlugin:
    """Dispatcher for `claude plugin ...` argv: records calls, serves
    `marketplace list --json`, returns rc=0 for everything else."""

    def __init__(self, marketplaces=None, list_stdout=None, list_rc=0,
                 fail_step=None, timeout_step=None, launch_fail=False):
        self.calls: list[list[str]] = []
        self.marketplaces = [] if marketplaces is None else marketplaces
        self.list_stdout = list_stdout
        self.list_rc = list_rc
        self.fail_step = fail_step        # e.g. "install", "add"
        self.timeout_step = timeout_step
        self.launch_fail = launch_fail

    def __call__(self, argv, capture_output=True, text=True, timeout=None):
        assert argv[:2] == ["claude", "plugin"]
        assert timeout is not None  # bounded subprocess calls, always
        step = argv[3] if argv[2] == "marketplace" else argv[2]
        self.calls.append(argv[2:])
        if self.launch_fail:
            raise OSError("claude binary missing")
        if self.timeout_step == step:
            raise subprocess.TimeoutExpired(argv, timeout)

        class _R:
            returncode = 0
            stdout = ""
            stderr = ""

        r = _R()
        if step == "list":
            r.returncode = self.list_rc
            r.stdout = (self.list_stdout if self.list_stdout is not None
                        else json.dumps(self.marketplaces))
        if self.fail_step == step:
            r.returncode = 1
            r.stderr = "boom"
        return r


def _patch_plugin_cmd(monkeypatch, fake):
    monkeypatch.setattr(cc.subprocess, "run", fake)


_PUBLISHED = [{"name": "hive", "source": "github", "repo": "notdp/hive"}]


def _published_config(monkeypatch, tmp_path, installed=True):
    cfg = tmp_path / "claude-cfg"
    (cfg / "plugins").mkdir(parents=True)
    if installed:
        (cfg / "plugins" / "installed_plugins.json").write_text(json.dumps(
            {"version": 2, "plugins": {"hive-channel@hive": [{"scope": "user"}]}}))
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(cfg))


def _no_mutations(fake):
    return all(c[0] != "install" and c[0] != "update"
               and c[:2] != ["marketplace", "add"]
               and c[:2] != ["marketplace", "remove"] for c in fake.calls)


# -- uninspectable binding: zero mutation (VAL-2.1) --

@pytest.mark.parametrize("fake_kwargs", [
    {"launch_fail": True},                 # claude binary missing
    {"timeout_step": "list"},              # list timeout
    {"list_rc": 1},                        # nonzero exit
    {"list_stdout": "not json"},           # malformed JSON
    {"list_stdout": json.dumps({"x": 1})}, # unknown top-level shape
])
def test_prepare_pane_uninspectable_list_fails_closed(
        _hive_home, tmp_path, monkeypatch, capsys, fake_kwargs):
    _published_config(monkeypatch, tmp_path, installed=True)
    fake = _FakeClaudePlugin(**fake_kwargs)
    _patch_plugin_cmd(monkeypatch, fake)
    assert cc.prepare_pane(str(tmp_path)) == []
    assert capsys.readouterr().err  # loud
    assert _no_mutations(fake)
    assert not (cc._channel_dir() / "marketplace").exists()


# -- inspected absence: one-time marketplace self-heal (VAL-2.2) --

def test_prepare_pane_absent_binding_adds_published_marketplace(
        _hive_home, tmp_path, monkeypatch):
    _published_config(monkeypatch, tmp_path, installed=False)
    fake = _FakeClaudePlugin(marketplaces=[])
    _patch_plugin_cmd(monkeypatch, fake)
    flags = cc.prepare_pane(str(tmp_path))
    assert flags == ["--channels", "plugin:hive-channel@hive"]
    assert fake.calls == [
        ["marketplace", "list", "--json"],
        ["marketplace", "add", "notdp/hive"],
        ["install", "hive-channel@hive"],
    ]


def test_prepare_pane_add_failure_stops_before_install(
        _hive_home, tmp_path, monkeypatch, capsys):
    _published_config(monkeypatch, tmp_path, installed=False)
    fake = _FakeClaudePlugin(marketplaces=[], fail_step="add")
    _patch_plugin_cmd(monkeypatch, fake)
    assert cc.prepare_pane(str(tmp_path)) == []
    assert "add" in capsys.readouterr().err
    assert all(c[0] != "install" for c in fake.calls)


def test_prepare_pane_never_touches_the_project_directory(tmp_path, monkeypatch, _hive_home):
    _published_config(monkeypatch, tmp_path / "cfg-root", installed=False)
    project = tmp_path / "project"
    project.mkdir()
    before = set(os.listdir(project))
    _patch_plugin_cmd(monkeypatch, _FakeClaudePlugin(marketplaces=[]))
    cc.prepare_pane(str(project))
    assert set(os.listdir(project)) == before  # no .mcp.json, nothing


# -- published binding: file-read fast path (VAL-2.3/2.4/2.6) --

def test_prepare_pane_published_binding_installed_is_subprocess_free(
        _hive_home, tmp_path, monkeypatch):
    # freshness belongs to auto-update / the bootstrap hook: an installed
    # plugin costs one marketplace-list probe, never install/update (a
    # github-marketplace `plugin update` git-fetches and cost 5-10s/launch)
    _published_config(monkeypatch, tmp_path, installed=True)
    fake = _FakeClaudePlugin(marketplaces=_PUBLISHED)
    _patch_plugin_cmd(monkeypatch, fake)
    flags = cc.prepare_pane(str(tmp_path))
    assert flags == ["--channels", "plugin:hive-channel@hive"]
    assert fake.calls == [["marketplace", "list", "--json"]]
    assert not (cc._channel_dir() / "marketplace").exists()


def test_prepare_pane_converged_repeat_stays_on_fast_path(
        _hive_home, tmp_path, monkeypatch):
    _published_config(monkeypatch, tmp_path, installed=True)
    fake = _FakeClaudePlugin(marketplaces=_PUBLISHED)
    _patch_plugin_cmd(monkeypatch, fake)
    first = cc.prepare_pane(str(tmp_path))
    second = cc.prepare_pane(str(tmp_path))
    assert first == second == ["--channels", "plugin:hive-channel@hive"]
    assert fake.calls == [["marketplace", "list", "--json"]] * 2
    assert not (cc._channel_dir() / "marketplace").exists()


def test_prepare_pane_published_binding_missing_plugin_self_heals_once(
        _hive_home, tmp_path, monkeypatch):
    _published_config(monkeypatch, tmp_path, installed=False)
    fake = _FakeClaudePlugin(marketplaces=_PUBLISHED)
    _patch_plugin_cmd(monkeypatch, fake)
    flags = cc.prepare_pane(str(tmp_path))
    assert flags == ["--channels", "plugin:hive-channel@hive"]
    assert ["install", "hive-channel@hive"] in fake.calls
    assert all(c[0] != "update" for c in fake.calls)
    assert all(c[:2] != ["marketplace", "add"] for c in fake.calls)


def test_prepare_pane_published_binding_fails_empty_on_install_failure(
        _hive_home, tmp_path, monkeypatch, capsys):
    _published_config(monkeypatch, tmp_path, installed=False)
    fake = _FakeClaudePlugin(marketplaces=_PUBLISHED, fail_step="install")
    _patch_plugin_cmd(monkeypatch, fake)
    assert cc.prepare_pane(str(tmp_path)) == []
    assert "install" in capsys.readouterr().err


# -- foreign occupants: fail loudly, zero mutation (VAL-2.5) --

@pytest.mark.parametrize("binding", [
    {"name": "hive", "source": "github", "repo": "someone-else/hive"},
    {"name": "hive", "source": "url", "repo": "notdp/hive"},   # exact-identity lookalike
    {"name": "hive", "source": "directory", "path": "/anywhere/marketplace"},  # legacy is foreign now
])
def test_prepare_pane_foreign_occupant_fails_closed(
        _hive_home, tmp_path, monkeypatch, capsys, binding):
    _published_config(monkeypatch, tmp_path, installed=True)
    fake = _FakeClaudePlugin(marketplaces=[binding])
    _patch_plugin_cmd(monkeypatch, fake)
    assert cc.prepare_pane(str(tmp_path)) == []
    err = capsys.readouterr().err
    assert "foreign" in err
    assert "marketplace remove hive" in err and "marketplace add notdp/hive" in err
    assert _no_mutations(fake)


def test_prepare_pane_fails_without_managed_allowlist(
        tmp_path, monkeypatch, capsys):
    # claude enforces the channels allowlist by silently dropping channel
    # notifications: without the policy entry the pane would be a deaf black
    # hole, so hive must refuse loudly with setup instructions instead
    fake = _FakeClaudePlugin()
    _patch_plugin_cmd(monkeypatch, fake)
    monkeypatch.setattr(cc, "_MANAGED_SETTINGS_PATHS",
                        (str(tmp_path / "absent.json"),))
    assert cc.prepare_pane(str(tmp_path)) == []
    err = capsys.readouterr().err
    assert "allowedChannelPlugins" in err  # setup hint present
    assert fake.calls == []  # nothing converged for an undeliverable pane


def test_prepare_pane_treats_malformed_policy_as_missing(
        tmp_path, monkeypatch, capsys):
    fake = _FakeClaudePlugin()
    _patch_plugin_cmd(monkeypatch, fake)
    policy = tmp_path / "managed-settings.json"
    policy.write_text("{broken")
    monkeypatch.setattr(cc, "_MANAGED_SETTINGS_PATHS", (str(policy),))
    assert cc.prepare_pane(str(tmp_path)) == []
    assert fake.calls == []


def test_prepare_pane_fails_when_channels_not_enabled(
        tmp_path, monkeypatch, capsys):
    # an allowlist entry without channelsEnabled is not the verified shape the
    # setup hint writes: refuse loudly instead of risking a deaf session
    fake = _FakeClaudePlugin()
    _patch_plugin_cmd(monkeypatch, fake)
    policy = tmp_path / "managed-settings.json"
    policy.write_text(json.dumps({"allowedChannelPlugins": [
        {"marketplace": "hive", "plugin": "hive-channel"}]}))
    monkeypatch.setattr(cc, "_MANAGED_SETTINGS_PATHS", (str(policy),))
    assert cc.prepare_pane(str(tmp_path)) == []
    assert "channelsEnabled" in capsys.readouterr().err  # hint shows the fix
    assert fake.calls == []


def test_prepare_pane_treats_non_object_policy_as_missing(
        tmp_path, monkeypatch):
    # valid JSON that is not an object must read as "no policy", not crash
    fake = _FakeClaudePlugin()
    _patch_plugin_cmd(monkeypatch, fake)
    policy = tmp_path / "managed-settings.json"
    policy.write_text(json.dumps(["not", "an", "object"]))
    monkeypatch.setattr(cc, "_MANAGED_SETTINGS_PATHS", (str(policy),))
    assert cc.prepare_pane(str(tmp_path)) == []
    assert fake.calls == []


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
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 0,
                                     "method": "initialize", "params": {}}) + "\n")
        proc.stdin.flush()
        assert json.loads(proc.stdout.readline())["id"] == 0
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


def test_server_publishes_no_marker_before_initialize(_hive_home):
    proc = _server_proc(_hive_home, "%80")
    sock_path = _hive_home / "channel" / "hive-pane-80.sock"
    marker = _hive_home / "channel" / "hive-pane-80.ready"
    try:
        assert _wait_path(sock_path)  # socket may listen early
        time.sleep(0.4)
        assert not marker.exists()  # but readiness waits for initialize
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 0,
                                     "method": "initialize", "params": {}}) + "\n")
        proc.stdin.flush()
        assert json.loads(proc.stdout.readline())["id"] == 0
        assert _wait_path(marker)
        assert marker.read_text() == cc.MARKER_RECEIPT_CAPABLE
    finally:
        proc.terminate()
        proc.wait(timeout=5)


def test_server_survives_legacy_client_and_serves_next_frame(_hive_home):
    """An old client closes before the receipt lands (BrokenPipe on the
    server's reply). The socket loop must survive and deliver a second frame."""
    pane = "%81"
    proc = _server_proc(_hive_home, pane)
    sock_path = _hive_home / "channel" / f"hive-pane-{pane.replace('%', '')}.sock"
    try:
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 0,
                                     "method": "initialize", "params": {}}) + "\n")
        proc.stdin.flush()
        assert json.loads(proc.stdout.readline())["id"] == 0
        assert _wait_path(sock_path)

        # frame 1: legacy-style client — write, half-close, then full-close
        # immediately without reading the receipt
        legacy = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        legacy.connect(str(sock_path))
        legacy.sendall(json.dumps({"msg_id": "old1", "content": "first"}).encode())
        legacy.shutdown(socket.SHUT_WR)
        legacy.close()
        note1 = json.loads(proc.stdout.readline())
        assert note1["method"] == "notifications/claude/channel"
        assert note1["params"]["meta"]["msg_id"] == "old1"

        # frame 2: new client — the loop must still be alive and answer
        assert cc.send_to_pane(pane, "<HIVE from=a to=b msgId=new2>second</HIVE>") == cc.ACCEPTED_MCP_WRITE
        note2 = json.loads(proc.stdout.readline())
        assert note2["params"]["meta"]["msg_id"] == "new2"
    finally:
        proc.terminate()
        proc.wait(timeout=5)


# --- atomic marker publication (VAL fail-r1 finding 2) ---


@pytest.fixture
def _srv_module(monkeypatch, _hive_home):
    """claude_channel_server with fresh readiness state per test."""
    from hive.adapters import claude_channel_server as srv

    monkeypatch.setattr(srv, "_initialized", threading.Event())
    monkeypatch.setattr(srv, "_socket_ready", threading.Event())
    monkeypatch.setattr(srv, "_marker_published", False)
    return srv


def test_marker_publish_is_atomic_and_never_empty(_srv_module, _hive_home, monkeypatch):
    """The visible marker path must never carry empty/partial content: the
    write goes to a temp file and lands via os.replace."""
    srv = _srv_module
    sock_path = srv.socket_path_for_pane("%60")
    Path(sock_path).parent.mkdir(parents=True, exist_ok=True)
    marker = Path(srv.marker_path_for_socket(sock_path))
    observed: list[str] = []

    real_replace = os.replace

    def _spy_replace(src, dst):
        # the instant before publication: the final path must not exist yet
        observed.append("exists-before" if marker.exists() else "absent-before")
        real_replace(src, dst)

    monkeypatch.setattr(srv.os, "replace", _spy_replace)
    srv._initialized.set()
    srv._socket_ready.set()
    srv._maybe_publish_marker(sock_path)

    assert observed == ["absent-before"]
    assert marker.read_text() == srv.MARKER_RECEIPT_CAPABLE
    assert not Path(str(marker) + ".tmp").exists()


def test_marker_publish_failure_is_retryable(_srv_module, _hive_home, monkeypatch):
    """A failed publish must not latch the published flag: the next gate
    event retries and succeeds."""
    srv = _srv_module
    sock_path = srv.socket_path_for_pane("%61")
    Path(sock_path).parent.mkdir(parents=True, exist_ok=True)
    marker = Path(srv.marker_path_for_socket(sock_path))

    calls = {"n": 0}
    real_replace = os.replace

    def _flaky_replace(src, dst):
        calls["n"] += 1
        if calls["n"] == 1:
            raise OSError("disk hiccup")
        real_replace(src, dst)

    monkeypatch.setattr(srv.os, "replace", _flaky_replace)
    srv._initialized.set()
    srv._socket_ready.set()

    srv._maybe_publish_marker(sock_path)
    assert not marker.exists()  # failed publish leaves no empty marker
    assert not Path(str(marker) + ".tmp").exists()  # temp cleaned
    assert srv._marker_published is False

    srv._maybe_publish_marker(sock_path)  # retry succeeds
    assert marker.read_text() == srv.MARKER_RECEIPT_CAPABLE
