"""The cross-session inbox transport: what actually reaches a Claude session.

The wire contract is fixed by Claude Code, not by hive: one newline-terminated
JSON object per message, ``type: "user"`` with a string ``content``, and
``priority: "now"`` for mid-turn delivery (the default parks it until the turn
ends, which is the turn-end drain this transport exists to replace). A frame
that drifts from that shape is dropped by the receiver without a word, so the
shape is asserted here rather than left to a live session to reveal.
"""
import json
import shutil
import socket
import tempfile
import threading

from pathlib import Path

import pytest

from hive.adapters import claude_uds as uds


@pytest.fixture
def inbox():
    """A stand-in session inbox: binds, accepts one connection, records lines.

    Rooted in /tmp, not pytest's tmp_path: a unix socket path over ~104 bytes
    cannot be bound at all."""
    root = Path(tempfile.mkdtemp(prefix="uds", dir="/tmp"))
    path = root / "s.sock"
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(path))
    srv.listen(1)
    received: list[str] = []

    def _serve():
        try:
            conn, _ = srv.accept()
        except OSError:
            return
        with conn:
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(4096)
                if not chunk:
                    break
                data += chunk
            received.append(data.decode())

    thread = threading.Thread(target=_serve, daemon=True)
    thread.start()
    yield str(path), received, thread
    srv.close()
    shutil.rmtree(root, ignore_errors=True)


def test_send_writes_one_mid_turn_user_frame(inbox):
    path, received, thread = inbox

    assert uds.send(path, "<HIVE from=validator msgId=m1>verdict</HIVE>") == uds.ACCEPTED_UDS_WRITE

    thread.join(timeout=5)
    assert len(received) == 1
    raw = received[0]
    assert raw.endswith("\n")  # the receiver frames on newlines
    frame = json.loads(raw)
    assert frame == {
        "type": "user",
        "priority": "now",
        "message": {"role": "user", "content": "<HIVE from=validator msgId=m1>verdict</HIVE>"},
    }


def test_send_fails_closed_on_a_socket_nobody_listens_to(tmp_path):
    """A socket file left by a killed session must not read as delivery."""
    corpse = tmp_path / "corpse.sock"
    corpse.touch()

    assert uds.send(str(corpse), "hi") is None
    assert uds.send("", "hi") is None
    assert uds.is_live(str(corpse)) is False
    assert uds.is_live("") is False


def test_is_live_follows_the_listener(inbox):
    path, _received, _thread = inbox
    assert uds.is_live(path) is True


def test_inbound_accepted_reads_the_user_settings_file(tmp_path, monkeypatch):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    settings = tmp_path / "settings.json"

    assert uds.inbound_accepted() is False  # missing file
    settings.write_text("{ not json")
    assert uds.inbound_accepted() is False
    settings.write_text(json.dumps({"crossSessionInbound": "hold"}))
    assert uds.inbound_accepted() is False
    settings.write_text(json.dumps({"crossSessionInbound": "accept"}))
    assert uds.inbound_accepted() is True


def test_session_socket_comes_from_the_host(monkeypatch):
    monkeypatch.delenv(uds.ENV_SOCKET, raising=False)
    assert uds.session_socket() == ""
    monkeypatch.setenv(uds.ENV_SOCKET, "/tmp/cc-socks/1.sock")
    assert uds.session_socket() == "/tmp/cc-socks/1.sock"
