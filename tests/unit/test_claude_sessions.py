"""Unit tests for the cross-session inbox adapter (registry + socket write)."""
import json
import os
import shutil
import socket
import tempfile
import threading
from pathlib import Path

import pytest

from hive.adapters import claude_sessions as m

pytestmark = pytest.mark.unit


@pytest.fixture
def short_tmp():
    # AF_UNIX sun_path caps near 104 bytes: sockets cannot live under pytest's
    # long tmp_path (the same reason production sockets live under $HIVE_HOME).
    base = "/tmp" if os.path.isdir("/tmp") else tempfile.gettempdir()
    d = Path(tempfile.mkdtemp(prefix="hive-cs-", dir=base))
    yield d
    shutil.rmtree(d, ignore_errors=True)


def _write_entry(root, fname, **fields):
    (root / "sessions").mkdir(parents=True, exist_ok=True)
    (root / "sessions" / fname).write_text(json.dumps(fields))


def _dead_pid() -> int:
    # a pid nothing is using, by the adapter's own liveness rule
    pid = 4_000_000
    while m._pid_alive(pid):
        pid += 1
    return pid


def test_list_sessions_keeps_only_live_entries_with_an_inbox(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    me = os.getpid()
    _write_entry(tmp_path, "1.json", name="alpha", pid=me, cwd="/w/a", kind="interactive", messagingSocketPath="/tmp/a.sock")
    _write_entry(tmp_path, "2.json", name="dead", pid=_dead_pid(), cwd="/w/d", kind="interactive", messagingSocketPath="/tmp/d.sock")
    _write_entry(tmp_path, "3.json", name="nosock", pid=me, cwd="/w/n", kind="interactive")
    _write_entry(tmp_path, "4.json", name="", pid=me, messagingSocketPath="/tmp/x.sock")
    (tmp_path / "sessions" / "5.json").write_text("{not json")
    (tmp_path / "sessions" / "6.json").write_text("[1, 2]")

    _write_entry(tmp_path, "7.json", name="spare", pid=me, cwd="/w/s", kind="interactive",
                 messagingSocketPath="/tmp/s.sock", spare=True)

    rows = m.list_sessions()

    assert [(s.name, s.pid, s.cwd, s.socket_path) for s in rows] == [("alpha", me, "/w/a", "/tmp/a.sock")]
    assert m.resolve("spare") == []  # a warm spare is nobody's address
    assert [s.name for s in m.resolve("alpha")] == ["alpha"]
    assert m.resolve("nosock") == []
    assert m.resolve("dead") == []


def _write_transcript(root, slug, session_id, lines):
    d = root / "projects" / slug
    d.mkdir(parents=True, exist_ok=True)
    (d / f"{session_id}.jsonl").write_text("\n".join(lines) + "\n")


def test_sessions_carry_the_desktop_title_and_answer_to_it(monkeypatch, tmp_path):
    # the desktop title lives in the transcript as a `custom-title` record; the
    # registry only knows the sessionId — join them so `hive msg` accepts what
    # the human actually sees in the sidebar
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    me = os.getpid()
    _write_entry(tmp_path, "1.json", name="nice-almeida-dd", pid=me, cwd="/w/a",
                 messagingSocketPath="/tmp/a.sock", sessionId="sid-a")
    _write_entry(tmp_path, "2.json", name="plain-b", pid=me, cwd="/w/b",
                 messagingSocketPath="/tmp/b.sock", sessionId="sid-b")
    _write_transcript(tmp_path, "-w-a", "sid-a", [
        json.dumps({"type": "custom-title", "customTitle": "old title", "sessionId": "sid-a"}),
        json.dumps({"type": "user", "message": {"role": "user", "content": "hi"}}),
        json.dumps({"type": "custom-title", "customTitle": "PR70 审查", "sessionId": "sid-a"}),
    ])
    _write_transcript(tmp_path, "-w-b", "sid-b", [json.dumps({"type": "user", "message": {"role": "user", "content": "x"}})])

    by_name = {s.name: s for s in m.list_sessions()}
    assert by_name["nice-almeida-dd"].title == "PR70 审查"  # the latest record wins
    assert by_name["plain-b"].title == ""
    assert [s.name for s in m.resolve("PR70 审查")] == ["nice-almeida-dd"]
    assert [s.name for s in m.resolve("nice-almeida-dd")] == ["nice-almeida-dd"]
    assert m.resolve("old title") == []


def test_session_title_scans_a_long_transcript_from_the_tail(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    filler = json.dumps({"type": "assistant", "message": {"content": "x" * 4000}})
    lines = [json.dumps({"type": "custom-title", "customTitle": "first", "sessionId": "sid-l"})]
    lines += [filler] * 300  # ~1.2 MB, well past the tail window
    lines += [json.dumps({"type": "custom-title", "customTitle": "current", "sessionId": "sid-l"})]
    lines += [filler] * 3
    _write_transcript(tmp_path, "-w-l", "sid-l", lines)
    assert m.session_title("sid-l") == "current"
    # a title set only at the start of a long session is still found
    lines2 = [json.dumps({"type": "custom-title", "customTitle": "early", "sessionId": "sid-e"})] + [filler] * 300
    _write_transcript(tmp_path, "-w-e", "sid-e", lines2)
    assert m.session_title("sid-e") == "early"
    assert m.session_title("") == ""
    assert m.session_title("sid-missing") == ""


def test_list_sessions_without_registry_dir_is_empty(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "missing"))
    assert m.list_sessions() == []


def test_registry_follows_claude_home_first(monkeypatch, tmp_path):
    # CLAUDE_HOME is hive's sandbox lever: a dev lane must never enumerate (or
    # message) the developer's real sessions
    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path / "lane"))
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "real"))
    _write_entry(tmp_path / "real", "1.json", name="real", pid=os.getpid(), messagingSocketPath="/tmp/r.sock")
    _write_entry(tmp_path / "lane", "2.json", name="lane", pid=os.getpid(), messagingSocketPath="/tmp/l.sock")
    assert [s.name for s in m.list_sessions()] == ["lane"]
    monkeypatch.delenv("CLAUDE_HOME")
    assert [s.name for s in m.list_sessions()] == ["real"]


def test_sessions_answer_to_their_pid(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    me = os.getpid()
    _write_entry(tmp_path, "1.json", name="worker", pid=me, cwd="/w/1", messagingSocketPath="/tmp/1.sock")
    assert [s.name for s in m.resolve(str(me))] == ["worker"]


def test_a_cleared_desktop_title_is_forgotten(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    _write_entry(tmp_path, "1.json", name="n", pid=os.getpid(), messagingSocketPath="/tmp/n.sock", sessionId="sid-c")
    _write_transcript(tmp_path, "-w-c", "sid-c", [
        json.dumps({"type": "custom-title", "customTitle": "was named", "sessionId": "sid-c"}),
        json.dumps({"type": "custom-title", "customTitle": "", "sessionId": "sid-c"}),
    ])
    assert m.list_sessions()[0].title == ""
    assert m.resolve("was named") == []


def test_resolve_returns_every_live_session_sharing_a_name(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    me = os.getpid()
    _write_entry(tmp_path, "1.json", name="worker", pid=me, cwd="/w/1", messagingSocketPath="/tmp/1.sock")
    _write_entry(tmp_path, "2.json", name="worker", pid=me, cwd="/w/2", messagingSocketPath="/tmp/2.sock")
    assert sorted(s.cwd for s in m.resolve("worker")) == ["/w/1", "/w/2"]


def test_send_writes_one_peer_message_line_and_reports_acceptance(short_tmp):
    path = str(short_tmp / "s.sock")
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(1)
    got: list[bytes] = []

    def _accept():
        conn, _ = srv.accept()
        with conn:
            buf = b""
            while not buf.endswith(b"\n"):
                chunk = conn.recv(4096)
                if not chunk:
                    break
                buf += chunk
            got.append(buf)

    t = threading.Thread(target=_accept, daemon=True)
    t.start()
    try:
        assert m.send(path, "hello there", sender="t.w") == m.ACCEPTED_UDS_WRITE
        t.join(timeout=5)
    finally:
        srv.close()

    assert len(got) == 1
    frame = json.loads(got[0].decode())
    assert frame == {
        "type": "user",
        "priority": "next",
        "from": "t.w",
        "message": {"role": "user", "content": "hello there"},
    }


def _listener(path: str):
    """A throwaway inbox listener on *path*; returns (srv, frames, thread)."""
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(1)
    got: list[bytes] = []

    def _accept():
        conn, _ = srv.accept()
        with conn:
            buf = b""
            while not buf.endswith(b"\n"):
                chunk = conn.recv(4096)
                if not chunk:
                    break
                buf += chunk
            got.append(buf)

    thread = threading.Thread(target=_accept, daemon=True)
    thread.start()
    return srv, got, thread


def test_send_carries_the_session_id_guard_only_when_given(short_tmp):
    # claude drops a frame whose session_id is not the target's own: that is
    # what keeps a recycled `<pid>.sock` from taking a dead session's mail.
    # With no id there is no guard — the frame must not carry an empty one.
    path = str(short_tmp / "g.sock")
    srv, got, thread = _listener(path)
    try:
        assert m.send(path, "x", sender="t.w", session_id="sid-1") == m.ACCEPTED_UDS_WRITE
        thread.join(timeout=5)
    finally:
        srv.close()
    assert json.loads(got[0].decode())["session_id"] == "sid-1"

    path = str(short_tmp / "n.sock")
    srv, got, thread = _listener(path)
    try:
        assert m.send(path, "x", sender="t.w") == m.ACCEPTED_UDS_WRITE
        thread.join(timeout=5)
    finally:
        srv.close()
    assert "session_id" not in json.loads(got[0].decode())


def test_send_to_a_dead_socket_is_none(short_tmp):
    assert m.send(str(short_tmp / "gone.sock"), "x", sender="hive") is None
    assert m.send("", "x", sender="hive") is None


def test_send_to_a_listener_that_never_reads_times_out_distinctly(short_tmp, monkeypatch):
    # accepted-but-stalled is reported apart from absent: the CLI words them
    # differently, and the second one may have left a truncated frame behind
    monkeypatch.setattr(m, "_WRITE_TIMEOUT", 0.3)
    path = str(short_tmp / "stall.sock")
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(1)
    try:
        # nobody calls accept()/recv(): the kernel accepts the connect into the
        # backlog and the socket buffers fill on a large enough frame
        assert m.send(path, "x" * 4_000_000, sender="hive") == m.WRITE_TIMED_OUT
    finally:
        srv.close()


def test_self_session_is_identified_by_its_own_socket(monkeypatch, tmp_path):
    # identity is the socket, never a saved slot: whichever live registration
    # names this process's own inbox is us
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    me = os.getpid()
    _write_entry(tmp_path, "1.json", name="mine", pid=me, messagingSocketPath="/tmp/mine.sock")
    _write_entry(tmp_path, "2.json", name="other", pid=me, messagingSocketPath="/tmp/other.sock")
    monkeypatch.setenv("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/mine.sock")
    assert m.self_session().name == "mine"
    monkeypatch.setenv("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/ghost.sock")
    assert m.self_session() is None
    monkeypatch.delenv("CLAUDE_CODE_MESSAGING_SOCKET")
    assert m.self_session() is None


def test_session_status_reports_only_live_tui_vocabulary(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path))
    me = os.getpid()
    _write_entry(tmp_path, f"{me}.json", name="w", pid=me, kind="interactive",
                 status="waiting", waitingFor="input needed")
    assert m.session_status(me) == ("waiting", "input needed")

    _write_entry(tmp_path, f"{me}.json", name="w", pid=me, kind="interactive",
                 status="busy")
    assert m.session_status(me) == ("busy", "")

    # `shell` is in the registry's own vocabulary — dropping it made a session
    # at its shell read as "nothing reported" and fall into the transcript gate
    _write_entry(tmp_path, f"{me}.json", name="w", pid=me, kind="interactive",
                 status="shell")
    assert m.session_status(me) == ("shell", "")

    # headless/desktop-hosted sessions never report status
    _write_entry(tmp_path, f"{me}.json", name="w", pid=me, kind="interactive")
    assert m.session_status(me) is None
    # unknown vocabulary is not trusted
    _write_entry(tmp_path, f"{me}.json", name="w", pid=me, status="warming")
    assert m.session_status(me) is None
    # dead process / missing entry / no pid
    dead = _dead_pid()
    _write_entry(tmp_path, f"{dead}.json", name="w", pid=dead, status="idle")
    assert m.session_status(dead) is None
    assert m.session_status(me + 1) is None
    assert m.session_status(None) is None


def test_runtime_from_status_maps_the_registry_vocabulary():
    assert m.runtime_from_status("busy") == {"busy": True, "inputState": "ready", "inputReason": ""}
    assert m.runtime_from_status("idle") == {"busy": False, "inputState": "ready", "inputReason": ""}
    # at its shell: not mid-turn, and not waiting on an answer either
    assert m.runtime_from_status("shell") == {"busy": False, "inputState": "ready", "inputReason": ""}
    assert m.runtime_from_status("waiting", "input needed") == {
        "busy": False, "inputState": "waiting_user", "inputReason": "registry:input needed",
    }
    assert m.runtime_from_status("waiting")["inputReason"] == "registry:unknown"
    assert m.runtime_from_status("")["inputState"] == "unknown"


def _control_server(path: str, replies: list[dict]):
    """A throwaway daemon control socket: answers one JSON line per
    connection from *replies* in order and records each received frame."""
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(4)
    got: list[dict] = []

    def _serve():
        for reply in replies:
            try:
                conn, _ = srv.accept()
            except OSError:
                return
            with conn:
                buf = b""
                while not buf.endswith(b"\n"):
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    buf += chunk
                got.append(json.loads(buf.decode()))
                conn.sendall((json.dumps(reply) + "\n").encode())

    t = threading.Thread(target=_serve, daemon=True)
    t.start()
    return srv, got, t


def _wire_daemon(monkeypatch, short_tmp, replies):
    sock_path = short_tmp / "control.sock"
    key = short_tmp / "daemon" / "control.key"
    key.parent.mkdir(parents=True)
    key.write_text("k3y\n")
    monkeypatch.setenv("CLAUDE_HOME", str(short_tmp))
    monkeypatch.setattr(m, "_daemon_control_sock", lambda: sock_path)
    monkeypatch.setattr(m, "_DAEMON_RETRY_DELAY", 0.01)
    return _control_server(str(sock_path), replies)


def test_daemon_reply_sends_the_exact_frame_and_reports_acceptance(monkeypatch, short_tmp):
    srv, got, t = _wire_daemon(monkeypatch, short_tmp, [{"ok": True, "op": "reply"}])
    try:
        out = m.daemon_reply("a65300e6-fed7-460f-ae17-9a94752d6fce", "<HIVE>hi</HIVE>")
        t.join(timeout=5)
    finally:
        srv.close()
    assert out == m.ACCEPTED_DAEMON_REPLY
    assert got == [
        {
            "proto": 1,
            "op": "reply",
            "short": "a65300e6",
            "auth": "k3y",
            "text": "<HIVE>hi</HIVE>",
        }
    ]


def test_daemon_reply_retries_readiness_codes_then_lands(monkeypatch, short_tmp):
    srv, got, t = _wire_daemon(
        monkeypatch,
        short_tmp,
        [
            {"ok": False, "code": "ESTARTING"},
            {"ok": False, "code": "ERESPAWNING"},
            {"ok": True, "op": "reply"},
        ],
    )
    try:
        out = m.daemon_reply("a65300e6-0000", "ping")
        t.join(timeout=5)
    finally:
        srv.close()
    assert out == m.ACCEPTED_DAEMON_REPLY
    assert len(got) == 3


def test_daemon_reply_does_not_retry_a_terminal_code(monkeypatch, short_tmp):
    srv, got, t = _wire_daemon(monkeypatch, short_tmp, [{"ok": False, "code": "ENOJOB"}])
    try:
        out = m.daemon_reply("a65300e6-0000", "ping")
        t.join(timeout=5)
    finally:
        srv.close()
    assert out is None
    assert len(got) == 1


def test_daemon_reply_rereads_the_key_once_on_eauth(monkeypatch, short_tmp):
    srv, got, t = _wire_daemon(
        monkeypatch,
        short_tmp,
        [{"ok": False, "code": "EAUTH"}, {"ok": False, "code": "EAUTH"}],
    )
    try:
        out = m.daemon_reply("a65300e6-0000", "ping")
        t.join(timeout=5)
    finally:
        srv.close()
    assert out is None
    assert len(got) == 2


def test_daemon_reply_without_a_daemon_is_none(monkeypatch, short_tmp):
    (short_tmp / "daemon").mkdir()
    (short_tmp / "daemon" / "control.key").write_text("k3y")
    monkeypatch.setenv("CLAUDE_HOME", str(short_tmp))
    monkeypatch.setattr(m, "_daemon_control_sock", lambda: short_tmp / "no.sock")
    assert m.daemon_reply("a65300e6-0000", "ping") is None


def test_daemon_reply_rejects_a_short_session_id(monkeypatch, short_tmp):
    assert m.daemon_reply("abc", "ping") is None
    assert m.daemon_reply("", "ping") is None


def test_daemon_control_sock_derives_from_the_config_dir(monkeypatch, tmp_path):
    import hashlib as _h

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    ns = _h.sha256(os.path.abspath(str(tmp_path)).encode()).hexdigest()[:8]
    assert m._daemon_control_sock() == Path("/tmp") / f"cc-daemon-{os.getuid()}" / ns / "control.sock"
