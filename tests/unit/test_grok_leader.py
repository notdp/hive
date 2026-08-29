"""Unit tests for the per-pane grok leader client.

The client is driven against a fake ``grok agent --leader stdio`` subprocess:
its stdin records the JSON-RPC lines hive writes and answers them through a
responder, its stdout is a real pipe the test feeds agent messages into. No
socket is bound and no process is spawned.
"""
import json
import os
import time
from pathlib import Path

import pytest

from hive.adapters import grok_leader as m

pytestmark = pytest.mark.unit

SID = "11111111-2222-3333-4444-555555555555"
CWD = "/w/project"


@pytest.fixture(autouse=True)
def _untagged_panes(monkeypatch):
    """Panes resolve to their raw pane key unless a test tags them.

    resolve_pane_key reads tmux pane options; without this pin the read
    would hit the real tmux binary — and tests that patch the global
    subprocess.Popen would swallow the tmux call itself.
    """
    monkeypatch.setattr("hive.tmux.get_pane_option", lambda pane, key: None)
    m._key_cache.clear()


# --------------------------------------------------------------------------
# fake subprocess
# --------------------------------------------------------------------------
class _Stdin:
    def __init__(self, proc: "FakeProc"):
        self._proc = proc
        self.lines: list[str] = []

    def write(self, text: str) -> int:
        self.lines.append(text)
        self._proc.on_write(text)
        return len(text)

    def flush(self) -> None:
        pass

    def close(self) -> None:
        pass


class FakeProc:
    def __init__(self, responder=None):
        read_fd, write_fd = os.pipe()
        self.stdout = os.fdopen(read_fd, "r")
        self._writer = os.fdopen(write_fd, "w")
        self.stdin = _Stdin(self)
        self.pid = 4321
        self.returncode = None
        self.responder = responder
        self.terminated = False

    def on_write(self, text: str) -> None:
        for reply in (self.responder(json.loads(text)) if self.responder else []):
            self.feed(reply)

    def poll(self):
        return self.returncode

    def terminate(self) -> None:
        self.terminated = True
        self.returncode = -15

    def wait(self, timeout=None):
        return self.returncode

    def feed(self, message: dict) -> None:
        self._writer.write(json.dumps(message) + "\n")
        self._writer.flush()

    def sent(self) -> list[dict]:
        return [json.loads(line) for line in list(self.stdin.lines)]

    def eof(self) -> None:
        try:
            self._writer.close()
        except (OSError, ValueError):
            pass


def _ok(msg: dict, result: dict | None = None) -> dict:
    return {"jsonrpc": "2.0", "id": msg["id"], "result": result or {}}


def responder(extra=None, replay=()):
    """Answers the handshake; `extra` handles everything else."""

    def respond(msg: dict) -> list[dict]:
        method = msg.get("method")
        if method == "initialize":
            return [_ok(msg, {"protocolVersion": 1})]
        if method == "session/load":
            return [*replay, _ok(msg, {"models": {"currentModelId": "grok-4.6"}})]
        return extra(msg) if extra else []

    return respond


@pytest.fixture
def grok_client(tmp_path, monkeypatch):
    """Factory: (responder) -> (client, fake proc) for pane %19."""
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    made: list[tuple] = []

    def make(respond=None, *, session=(SID, CWD), pane="%19"):
        if session is not None:
            m.write_pane_session(pane, *session)
        proc = FakeProc(respond)
        monkeypatch.setattr(m.subprocess, "Popen", lambda *a, **k: proc)
        client = m.GrokStdioClient(pane)
        made.append((client, proc))
        return client, proc

    yield make
    for client, proc in made:
        client._closed = True
        proc.eof()
        client._reader.join(timeout=1.0)
        proc.stdout.close()


def _loaded(make, respond=None, replay=()):
    client, proc = make(respond or responder(replay=replay))
    assert client.handshake() is True
    return client, proc


def _settle(client, predicate, timeout=2.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        runtime = client.runtime()
        if runtime is not None and predicate(runtime):
            return runtime
        time.sleep(0.005)
    raise AssertionError(f"runtime never matched: {client.runtime()}")


def _settle_sent(proc, predicate, timeout=2.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for msg in proc.sent():
            if predicate(msg):
                return msg
        time.sleep(0.005)
    raise AssertionError(f"no matching write: {proc.sent()}")


def _update(kind: str, session_id: str = SID, **fields) -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session_id, "update": {"sessionUpdate": kind, **fields}},
    }


def _activity(activity: str, session_id: str = SID) -> dict:
    return {
        "jsonrpc": "2.0",
        "method": "_x.ai/sessions/changed",
        "params": {"upserted": [{"sessionId": session_id, "activity": activity, "resident": True}]},
    }


# --------------------------------------------------------------------------
# handshake
# --------------------------------------------------------------------------
def test_handshake_sends_initialize_then_session_load(grok_client):
    client, proc = _loaded(grok_client)
    sent = proc.sent()
    assert [msg["method"] for msg in sent] == ["initialize", "session/load"]
    assert sent[0]["params"] == {
        "protocolVersion": 1,
        "clientInfo": {"name": "hive", "version": "1"},
        "clientCapabilities": {},
    }
    assert sent[1]["params"] == {"sessionId": SID, "cwd": CWD, "mcpServers": []}


def test_handshake_stops_without_pane_session_file(grok_client):
    client, proc = grok_client(responder(), session=None)
    assert client.handshake() is False
    assert proc.sent() == []


def test_handshake_false_when_load_errors(grok_client):
    def respond(msg):
        if msg.get("method") == "initialize":
            return [_ok(msg, {"protocolVersion": 1})]
        return [{"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32602, "message": "unknown session id"}}]

    client, _proc = grok_client(respond)
    assert client.handshake() is False


def test_notifications_before_load_response_are_discarded(grok_client):
    replay = [
        _update("agent_message_chunk", content={"type": "text", "text": "old turn"}),
        _activity("working"),
    ]
    client, _proc = _loaded(grok_client, replay=replay)
    assert client.runtime() is None  # replay is not evidence of a live turn


def test_notification_right_behind_the_load_response_is_folded(grok_client):
    """A live turn queued behind the load response must not count as replay."""

    def respond(msg):
        if msg.get("method") == "initialize":
            return [_ok(msg, {"protocolVersion": 1})]
        if msg.get("method") == "session/load":
            return [_ok(msg, {"models": {"currentModelId": "grok-4.6"}}), _activity("working")]
        return []

    client, _proc = grok_client(respond)
    assert client.handshake() is True
    _settle(client, lambda rt: rt.busy)


def test_handshake_fails_fast_when_the_child_dies(grok_client, monkeypatch):
    monkeypatch.setattr(m, "_INIT_TIMEOUT", 5.0)
    holder: dict = {}

    def respond(_msg):
        holder["proc"].eof()  # the stdio child dies instead of answering
        return []

    client, proc = grok_client(respond)
    holder["proc"] = proc
    started = time.monotonic()
    assert client.handshake() is False
    assert time.monotonic() - started < 1.0  # death, not the initialize timeout


# --------------------------------------------------------------------------
# notification folding
# --------------------------------------------------------------------------
def test_activity_working_marks_busy_and_idle_closes_turn(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed(_activity("working"))
    assert _settle(client, lambda rt: rt.busy).session_id == SID
    proc.feed(_activity("idle"))
    runtime = _settle(client, lambda rt: not rt.busy)
    assert runtime.turn_phase == "turn_closed"
    assert runtime.input_state == "ready"


def test_message_chunks_mark_user_prompt_pending(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed(_update("agent_thought_chunk", content={"type": "text", "text": "The"}))
    runtime = _settle(client, lambda rt: rt.busy)
    assert runtime.turn_phase == "user_prompt_pending"


def test_tool_call_phases_survive_streamed_chunks(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed(_update("tool_call", toolCallId="c1", status="pending"))
    assert _settle(client, lambda rt: rt.turn_phase == "tool_open").busy
    proc.feed(_update("tool_call_update", toolCallId="c1", status="completed"))
    _settle(client, lambda rt: rt.turn_phase == "tool_result_pending_reply")
    proc.feed(_update("agent_message_chunk", content={"type": "text", "text": "done"}))
    time.sleep(0.05)
    assert client.runtime().turn_phase == "tool_result_pending_reply"


def test_late_joined_tool_call_update_marks_busy(grok_client):
    # attaching mid-tool: the opening tool_call was never seen, the update is
    # the only evidence that a turn is running
    client, proc = _loaded(grok_client)
    proc.feed(_update("tool_call_update", toolCallId="c1", status="in_progress"))
    _settle(client, lambda rt: rt.busy)


def test_tool_call_update_clears_a_decided_permission(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed({
        "jsonrpc": "2.0",
        "id": 78,
        "method": "session/request_permission",
        "params": {"sessionId": SID, "toolCall": {"toolCallId": "c1"}, "options": []},
    })
    _settle(client, lambda rt: rt.input_state == "waiting_user")
    # the human answered at the TUI: the tool ran, so nothing waits on input
    proc.feed(_update("tool_call_update", toolCallId="c1", status="completed"))
    _settle(client, lambda rt: rt.input_state == "ready")


def test_turn_completed_clears_busy(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed(_activity("working"))
    _settle(client, lambda rt: rt.busy)
    proc.feed({
        "jsonrpc": "2.0",
        "method": "_x.ai/session_notification",
        "params": {"sessionId": SID, "update": {"sessionUpdate": "turn_completed", "stop_reason": "end_turn"}},
    })
    runtime = _settle(client, lambda rt: not rt.busy)
    assert runtime.turn_phase == "turn_closed"
    assert runtime.input_state == "ready"


def test_queued_entries_mark_input_backlog(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed({
        "jsonrpc": "2.0",
        "method": "_x.ai/queue/changed",
        "params": {"sessionId": SID, "entries": [{"id": "p1", "kind": "prompt", "text": "next", "position": 0}]},
    })
    _settle(client, lambda rt: rt.turn_phase == "input_backlog")


def test_other_session_notifications_are_ignored(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed(_update("tool_call", toolCallId="c1", status="pending"))
    baseline = _settle(client, lambda rt: rt.turn_phase == "tool_open")
    proc.feed(_activity("idle", session_id="other-session"))
    proc.feed(_update("agent_message_chunk", session_id="other-session", content={"text": "hi"}))
    # same-session no-op marker: the reader folds it only after the two lines
    # above, so its observed_at bump proves they were seen and dropped
    proc.feed(_activity("working"))
    runtime = _settle(client, lambda rt: rt.observed_at > baseline.observed_at)
    assert runtime.busy is True
    assert runtime.turn_phase == "tool_open"  # the foreign idle never closed it
    assert runtime.input_state == ""


def test_unknown_updates_are_ignored(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed(_update("available_commands_update", availableCommands=[{"name": "compact"}]))
    first = _settle(client, lambda rt: True)
    # the second ignored line is its own marker: an in-session notification
    # bumps observed_at even when nothing folds it
    proc.feed({"jsonrpc": "2.0", "method": "_x.ai/announcements/update", "params": {"sessionId": SID}})
    runtime = _settle(client, lambda rt: rt.observed_at > first.observed_at)
    assert runtime.busy is False
    assert runtime.turn_phase == "unknown_evidence"


# --------------------------------------------------------------------------
# prompt delivery
# --------------------------------------------------------------------------
def test_prompt_acks_on_queue_changed_echo(grok_client):
    def on_prompt(msg):
        if msg.get("method") != "session/prompt":
            return []
        text = msg["params"]["prompt"][0]["text"]
        return [{
            "jsonrpc": "2.0",
            "method": "_x.ai/queue/changed",
            "params": {"sessionId": SID, "entries": [{"id": "p1", "kind": "prompt", "text": text, "position": 0}]},
        }]

    client, proc = _loaded(grok_client, responder(extra=on_prompt))
    assert client.prompt("hello grok") is True
    prompt_msg = proc.sent()[-1]
    assert prompt_msg["method"] == "session/prompt"
    assert prompt_msg["params"] == {
        "sessionId": SID,
        "prompt": [{"type": "text", "text": "hello grok"}],
    }


def test_prompt_acks_on_running_text_echo(grok_client):
    def on_prompt(msg):
        if msg.get("method") != "session/prompt":
            return []
        return [{
            "jsonrpc": "2.0",
            "method": "_x.ai/queue/changed",
            "params": {"sessionId": SID, "entries": [], "runningText": "hello grok", "runningKind": "prompt"},
        }]

    client, _proc = _loaded(grok_client, responder(extra=on_prompt))
    assert client.prompt("hello grok") is True


def test_prompt_acks_on_user_message_chunk(grok_client):
    def on_prompt(msg):
        if msg.get("method") != "session/prompt":
            return []
        text = msg["params"]["prompt"][0]["text"]
        return [_update("user_message_chunk", content={"type": "text", "text": text})]

    client, _proc = _loaded(grok_client, responder(extra=on_prompt))
    assert client.prompt("hello grok") is True


def test_prompt_false_on_error_response(grok_client):
    def on_prompt(msg):
        if msg.get("method") != "session/prompt":
            return []
        return [{"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32602, "message": "unknown session id"}}]

    client, _proc = _loaded(grok_client, responder(extra=on_prompt))
    assert client.prompt("hello grok") is False


def test_prompt_false_when_never_acked(grok_client, monkeypatch):
    monkeypatch.setattr(m, "_ACK_TIMEOUT", 0.05)
    client, _proc = _loaded(grok_client)  # nothing answers session/prompt
    assert client.prompt("hello grok") is False


def test_prompt_echo_of_another_text_does_not_ack(grok_client, monkeypatch):
    monkeypatch.setattr(m, "_ACK_TIMEOUT", 0.05)

    def on_prompt(msg):
        if msg.get("method") != "session/prompt":
            return []
        return [_update("user_message_chunk", content={"type": "text", "text": "someone else"})]

    client, _proc = _loaded(grok_client, responder(extra=on_prompt))
    assert client.prompt("hello grok") is False


# --------------------------------------------------------------------------
# permission requests
# --------------------------------------------------------------------------
def test_permission_request_is_cancelled_and_marks_waiting_user(grok_client):
    client, proc = _loaded(grok_client)
    proc.feed({
        "jsonrpc": "2.0",
        "id": 77,
        "method": "session/request_permission",
        "params": {
            "sessionId": SID,
            "toolCall": {"toolCallId": "c1", "title": "rm -rf"},
            "options": [{"optionId": "a", "name": "Allow", "kind": "allow_once"}],
        },
    })
    answer = _settle_sent(proc, lambda msg: msg.get("id") == 77)
    assert answer["result"] == {"outcome": {"outcome": "cancelled"}}
    _settle(client, lambda rt: rt.input_state == "waiting_user")


# --------------------------------------------------------------------------
# interrupt
# --------------------------------------------------------------------------
def test_cancel_writes_a_bare_notification_for_the_session(grok_client):
    # ACP cancel is a notification: the leader answers a cancel carrying an
    # id with -32601 and keeps running the turn, so the write must have no id.
    client, proc = _loaded(grok_client)
    assert client.cancel() is True
    cancel = proc.sent()[-1]
    assert cancel["method"] == "session/cancel"
    assert cancel["params"] == {"sessionId": SID}
    assert "id" not in cancel


def test_cancel_false_without_a_loaded_session(grok_client):
    client, proc = grok_client(responder())  # no handshake -> no session bound
    assert client.cancel() is False
    assert not any(msg.get("method") == "session/cancel" for msg in proc.sent())


def test_cancel_false_when_the_pipe_is_dead(grok_client):
    client, proc = _loaded(grok_client)

    def broken(_text):
        raise OSError("broken pipe")

    proc.stdin.write = broken
    assert client.cancel() is False


# --------------------------------------------------------------------------
# compaction
# --------------------------------------------------------------------------
def test_compact_returns_compacted_when_idle(grok_client):
    def on_compact(msg):
        return [_ok(msg)] if msg.get("method") == "x.ai/compact_conversation" else []

    client, proc = _loaded(grok_client, responder(extra=on_compact))
    assert client.compact() == "compacted"
    assert proc.sent()[-1]["params"] == {"sessionId": SID}


def test_compact_defers_while_busy(grok_client):
    def on_compact(msg):
        if msg.get("method") == "x.ai/compact_conversation":
            raise AssertionError("must not compact a busy session")
        return []

    client, proc = _loaded(grok_client, responder(extra=on_compact))
    proc.feed(_activity("working"))
    _settle(client, lambda rt: rt.busy)
    assert client.compact() == "busy"


def test_compact_unavailable_on_error(grok_client):
    def on_compact(msg):
        if msg.get("method") != "x.ai/compact_conversation":
            return []
        return [{"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "unsupported"}}]

    client, _proc = _loaded(grok_client, responder(extra=on_compact))
    assert client.compact() == "unavailable"


# --------------------------------------------------------------------------
# process lifecycle
# --------------------------------------------------------------------------
def test_client_close_terminates_the_subprocess(grok_client):
    client, proc = grok_client(responder())
    assert client.is_alive() is True
    client.close()
    assert proc.terminated is True
    assert client.is_alive() is False


def test_client_dies_on_stdout_eof(grok_client):
    client, proc = _loaded(grok_client)
    proc.eof()
    deadline = time.monotonic() + 2.0
    while client.is_alive() and time.monotonic() < deadline:
        time.sleep(0.005)
    assert client.is_alive() is False


def test_stdio_argv_targets_the_pane_socket(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    m.write_pane_session("%19", SID, CWD)
    seen: dict = {}
    proc = FakeProc(responder())

    def fake_popen(argv, **kwargs):
        seen["argv"] = argv
        seen["kwargs"] = kwargs
        return proc

    monkeypatch.setattr(m.subprocess, "Popen", fake_popen)
    client = m.GrokStdioClient("%19")
    try:
        assert seen["argv"] == [
            "grok", "agent", "--leader", "stdio",
            "--leader-socket", str(m.pane_socket_path("%19")),
        ]
        assert seen["kwargs"]["text"] is True
        assert seen["kwargs"]["bufsize"] == 1
        assert seen["kwargs"]["stderr"] == m.subprocess.DEVNULL
    finally:
        client._closed = True
        proc.eof()
        client._reader.join(timeout=1.0)
        proc.stdout.close()


# --------------------------------------------------------------------------
# paths and pane session records
# --------------------------------------------------------------------------
def test_pane_socket_path_under_grok_home(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    path = m.pane_socket_path("%19")
    assert path.parent.name == "hive"
    assert str(path).endswith("hive/p19.sock")


def test_pane_socket_path_stays_under_unix_limit(monkeypatch):
    monkeypatch.delenv("GROK_HOME", raising=False)
    assert len(str(m.pane_socket_path("%19"))) < 104


def test_sibling_paths_share_the_socket_stem(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    assert m.pane_pidfile_path("%19").name == "p19.pid"
    assert m.pane_session_path("%19").name == "p19.session"


def test_pane_session_round_trip(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    m.write_pane_session("%19", SID, CWD)
    assert m.read_pane_session("%19") == (SID, CWD)
    assert m.session_id_for_pane("%19") == SID


def test_read_pane_session_none_when_missing_or_invalid(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    assert m.read_pane_session("%19") is None
    assert m.session_id_for_pane("%19") is None
    path = m.pane_session_path("%19")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not json")
    assert m.read_pane_session("%19") is None
    path.write_text(json.dumps({"sessionId": SID}))
    assert m.read_pane_session("%19") is None
    path.write_text(json.dumps(["not", "a", "dict"]))
    assert m.read_pane_session("%19") is None


def test_key_from_socket_name_roundtrip():
    assert m._key_from_socket_name("p19.sock") == "p19"
    assert m._key_from_socket_name("m-honey.rex.sock") == "m-honey.rex"
    assert m._key_from_socket_name("m-honey.rex.dot.sock") == "m-honey.rex.dot"
    assert m._key_from_socket_name("pdefault.sock") is None
    assert m._key_from_socket_name("m-noseparator.sock") is None
    assert m._key_from_socket_name("p19.pid") is None
    assert m._key_from_socket_name("leader.sock") is None


def test_member_key_roundtrip():
    assert m.member_key("honey", "rex") == "m-honey.rex"
    assert m.member_from_key("m-honey.rex") == ("honey", "rex")
    # member names may carry dots; team names are dot-free, so the first
    # dot is the separator.
    assert m.member_from_key("m-honey.rex.two") == ("honey", "rex.two")
    assert m.member_from_key("p19") is None
    assert m.member_from_key("m-") is None


def test_resolve_pane_key_uses_member_tags(monkeypatch):
    tags = {("%9", "hive-team"): "honey", ("%9", "hive-agent"): "rex"}
    monkeypatch.setattr(
        "hive.tmux.get_pane_option", lambda pane, key: tags.get((pane, key))
    )
    m._key_cache.clear()
    assert m.resolve_pane_key("%9") == "m-honey.rex"
    assert m.resolve_pane_key("%7") == "p7"  # untagged: raw pane lifecycle
    m._key_cache.clear()


def test_list_daemon_keys_filters_to_daemon_sockets(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    hive_dir = tmp_path / "hive"
    hive_dir.mkdir()
    (hive_dir / "p19.sock").touch()
    (hive_dir / "p7.sock").touch()
    (hive_dir / "m-honey.rex.sock").touch()
    (hive_dir / "pdefault.sock").touch()
    (hive_dir / "p19.session").touch()
    assert sorted(m.list_daemon_keys()) == ["m-honey.rex", "p19", "p7"]


def test_list_daemon_keys_missing_dir(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    assert m.list_daemon_keys() == []


# --------------------------------------------------------------------------
# daemon lifecycle
# --------------------------------------------------------------------------
def test_probe_socket_needs_socket_and_live_pid(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    sock = m.pane_socket_path("%19")
    sock.parent.mkdir(parents=True, exist_ok=True)
    assert m.probe_socket(str(sock)) is False  # no socket
    sock.touch()
    assert m.probe_socket(str(sock)) is False  # no pidfile
    m.pane_pidfile_path("%19").write_text(str(os.getpid()))
    assert m.probe_socket(str(sock)) is True

    def dead(_pid, _sig):
        raise ProcessLookupError

    monkeypatch.setattr(m.os, "kill", dead)
    assert m.probe_socket(str(sock)) is False


def test_spawn_daemon_builds_leader_argv_and_pane_env(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    monkeypatch.setenv("TMUX_PANE", "%old")
    seen: dict = {}

    class Child:
        pid = 7777

        def poll(self):
            return None

        def terminate(self):
            raise AssertionError("must not terminate a healthy leader")

    def fake_popen(argv, **kwargs):
        seen["argv"] = argv
        seen["kwargs"] = kwargs
        Path(argv[argv.index("--leader-socket") + 1]).touch()
        return Child()

    monkeypatch.setattr(m.subprocess, "Popen", fake_popen)
    assert m.spawn_daemon("%19") is True
    assert seen["argv"] == [
        "grok", "agent", "leader",
        "--leader-socket", str(m.pane_socket_path("%19")),
        "--no-auto-update",
        "--no-exit-on-disconnect",
    ]
    assert seen["kwargs"]["env"]["TMUX_PANE"] == "%19"
    assert seen["kwargs"]["start_new_session"] is True
    assert seen["kwargs"]["stdin"] == m.subprocess.DEVNULL
    assert m.pane_pidfile_path("%19").read_text() == "7777"


def test_spawn_daemon_false_when_leader_exits_early(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))

    class DeadChild:
        pid = 7778

        def poll(self):
            return 1

        def terminate(self):
            pass

    monkeypatch.setattr(m.subprocess, "Popen", lambda *a, **k: DeadChild())
    assert m.spawn_daemon("%19") is False
    assert not m.pane_pidfile_path("%19").exists()


def test_spawn_daemon_reuses_a_live_daemon(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    sock = m.pane_socket_path("%19")
    sock.parent.mkdir(parents=True, exist_ok=True)
    sock.touch()
    m.pane_pidfile_path("%19").write_text(str(os.getpid()))

    def no_spawn(*_a, **_k):
        raise AssertionError("must not respawn a live leader")

    monkeypatch.setattr(m.subprocess, "Popen", no_spawn)
    assert m.spawn_daemon("%19") is True


def test_spawn_daemon_clears_a_stale_socket(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    sock = m.pane_socket_path("%19")
    sock.parent.mkdir(parents=True, exist_ok=True)
    sock.touch()  # stale: no pidfile, so no live daemon
    seen: dict = {}

    class Child:
        pid = 7779

        def poll(self):
            return None

        def terminate(self):
            pass

    def fake_popen(argv, **kwargs):
        seen["existed"] = sock.exists()
        Path(argv[argv.index("--leader-socket") + 1]).touch()
        return Child()

    monkeypatch.setattr(m.subprocess, "Popen", fake_popen)
    assert m.spawn_daemon("%19") is True
    assert seen["existed"] is False  # stale socket unlinked before respawn


def test_kill_pane_daemon_removes_socket_pid_and_session(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    m.write_pane_session("%19", SID, CWD)
    m.pane_socket_path("%19").touch()
    m.pane_pidfile_path("%19").write_text("4321")
    killed: list[int] = []
    monkeypatch.setattr(m, "_terminate_process_group", killed.append)
    m.kill_pane_daemon("%19")
    assert killed == [4321]
    assert not m.pane_socket_path("%19").exists()
    assert not m.pane_pidfile_path("%19").exists()
    assert not m.pane_session_path("%19").exists()


# --------------------------------------------------------------------------
# pool
# --------------------------------------------------------------------------
def test_pool_send_to_pane_returns_prompt_queued(monkeypatch):
    grok_pool = m.GrokClientPool()
    sent: list[str] = []

    class FakeClient:
        def prompt(self, text):
            sent.append(text)
            return True

    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: FakeClient())
    assert grok_pool.send_to_pane("%19", "hi") == m.PROMPT_QUEUED
    assert sent == ["hi"]


def test_pool_send_to_pane_none_without_client(monkeypatch):
    grok_pool = m.GrokClientPool()
    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: None)
    assert grok_pool.send_to_pane("%19", "hi") is None


def test_pool_send_to_pane_none_when_client_raises(monkeypatch):
    grok_pool = m.GrokClientPool()

    class FakeClient:
        def prompt(self, _text):
            raise OSError("broken pipe")

    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: FakeClient())
    assert grok_pool.send_to_pane("%19", "hi") is None


def test_pool_interrupt_pane_returns_cancel_sent(monkeypatch):
    grok_pool = m.GrokClientPool()
    cancelled = []

    class FakeClient:
        def cancel(self):
            cancelled.append(True)
            return True

    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: FakeClient())
    assert grok_pool.interrupt_pane("%19") == m.CANCEL_SENT
    assert cancelled == [True]


def test_pool_interrupt_pane_none_without_client(monkeypatch):
    grok_pool = m.GrokClientPool()
    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: None)
    assert grok_pool.interrupt_pane("%19") is None


def test_pool_interrupt_pane_none_when_the_write_fails(monkeypatch):
    grok_pool = m.GrokClientPool()

    class FakeClient:
        def cancel(self):
            return False

    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: FakeClient())
    assert grok_pool.interrupt_pane("%19") is None


def test_pool_interrupt_pane_none_when_client_raises(monkeypatch):
    grok_pool = m.GrokClientPool()

    class FakeClient:
        def cancel(self):
            raise OSError("broken pipe")

    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: FakeClient())
    assert grok_pool.interrupt_pane("%19") is None


def test_pool_compact_pane_unavailable_without_client(monkeypatch):
    grok_pool = m.GrokClientPool()
    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: None)
    assert grok_pool.compact_pane("%19") == "unavailable"


def test_pool_runtime_for_pane_none_without_client(monkeypatch):
    grok_pool = m.GrokClientPool()
    monkeypatch.setattr(grok_pool, "_client_for", lambda _pane: None)
    assert grok_pool.runtime_for_pane("%19") is None
    assert grok_pool.connect("%19") is False


def test_pool_skips_panes_without_socket_or_session(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    monkeypatch.setattr(m.subprocess, "Popen", lambda *a, **k: pytest.fail("no client without a daemon"))
    grok_pool = m.GrokClientPool()
    assert grok_pool._client_for("%19") is None  # no socket at all
    sock = m.pane_socket_path("%19")
    sock.parent.mkdir(parents=True, exist_ok=True)
    sock.touch()
    grok_pool._cooldown.clear()
    assert grok_pool._client_for("%19") is None  # socket but no session record


def test_pool_skips_a_pane_whose_leader_pid_is_dead(tmp_path, monkeypatch):
    # a socket file outlives the leader that bound it: connecting to it hangs
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    m.write_pane_session("%19", SID, CWD)
    m.pane_socket_path("%19").touch()
    m.pane_pidfile_path("%19").write_text("999999")
    monkeypatch.setattr(
        m.subprocess, "Popen", lambda *a, **k: pytest.fail("no client without a live leader")
    )
    assert m.GrokClientPool()._client_for("%19") is None


def test_pool_rebinds_when_the_pane_session_record_rotates(tmp_path, monkeypatch):
    # grok relaunched in the same pane mints a new session id; the client bound
    # to the old one would report a stale session forever
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    m.write_pane_session("%19", SID, CWD)
    m.pane_socket_path("%19").touch()
    m.pane_pidfile_path("%19").write_text(str(os.getpid()))
    procs: list[FakeProc] = []

    def fake_popen(*_a, **_k):
        procs.append(FakeProc(responder()))
        return procs[-1]

    monkeypatch.setattr(m.subprocess, "Popen", fake_popen)
    grok_pool = m.GrokClientPool()
    clients: list = []

    def bind():
        client = grok_pool._client_for("%19")
        if client is not None and client not in clients:
            clients.append(client)
        return client

    try:
        first = bind()
        assert first is not None and first.session_id == SID
        assert bind() is first  # stable while the record holds

        rotated = "99999999-8888-7777-6666-555555555555"
        m.write_pane_session("%19", rotated, CWD)
        second = bind()
        assert second is not first
        assert second.session_id == rotated
        assert first.is_alive() is False  # the stale client is closed, not leaked
    finally:
        grok_pool.drop("%19")
        for proc in procs:
            proc.eof()
        for client in clients:
            client._reader.join(timeout=1.0)
        for proc in procs:
            proc.stdout.close()


def test_daemon_env_washes_inherited_identity_markers(monkeypatch):
    """Regression: a leader spawned from inside another member's engine
    inherited that engine's CLAUDE_CODE_MESSAGING_SOCKET, so every hive call
    in this grok member resolved to the orch's pane (replies came from=orch)."""
    from hive.adapters import grok_leader

    monkeypatch.setenv("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/cc-socks/999.sock")
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", "/tmp/elsewhere")
    monkeypatch.setenv("CODEX_THREAD_ID", "tid-1")
    monkeypatch.setenv("TMUX_PANE", "%stale")

    env = grok_leader._daemon_env_for_pane("%42")

    assert env["TMUX_PANE"] == "%42"
    assert "CLAUDE_CODE_MESSAGING_SOCKET" not in env
    assert "CLAUDE_CONFIG_DIR" not in env
    assert "CODEX_THREAD_ID" not in env


def test_spawn_daemon_member_pane_gets_member_socket_and_identity_env(tmp_path, monkeypatch):
    """A tagged member pane spawns a member-keyed daemon whose env carries the
    member identity — and never the spawner's inherited one."""
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    monkeypatch.setenv("HIVE_TEAM", "spawner-team")
    monkeypatch.setenv("HIVE_MEMBER", "spawner")
    tags = {("%19", "hive-team"): "honey", ("%19", "hive-agent"): "rex"}
    monkeypatch.setattr(
        "hive.tmux.get_pane_option", lambda pane, key: tags.get((pane, key))
    )
    m._key_cache.clear()
    seen: dict = {}

    class Child:
        pid = 7777

        def poll(self):
            return None

    def fake_popen(argv, **kwargs):
        seen["argv"] = argv
        seen["kwargs"] = kwargs
        Path(argv[argv.index("--leader-socket") + 1]).touch()
        return Child()

    monkeypatch.setattr(m.subprocess, "Popen", fake_popen)
    try:
        assert m.spawn_daemon("%19") is True
        sock = argv_sock = seen["argv"][seen["argv"].index("--leader-socket") + 1]
        assert argv_sock.endswith("m-honey.rex.sock")
        env = seen["kwargs"]["env"]
        assert env["HIVE_TEAM"] == "honey"
        assert env["HIVE_MEMBER"] == "rex"
        assert env["TMUX_PANE"] == "%19"
        assert (tmp_path / "hive" / "m-honey.rex.pid").read_text() == "7777"
        assert sock == str(m.socket_path_for_key("m-honey.rex"))
    finally:
        m._key_cache.clear()


def test_kill_daemon_key_removes_socket_pid_and_session(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    sock = m.socket_path_for_key("m-honey.rex")
    sock.parent.mkdir(parents=True, exist_ok=True)
    sock.touch()
    sock.with_suffix(".pid").write_text("4321")
    sock.with_suffix(".session").write_text('{"sessionId": "s", "cwd": "/c"}')
    killed: list = []
    monkeypatch.setattr(m, "_terminate_process_group", lambda pid: killed.append(pid))

    m.kill_daemon_key("m-honey.rex")

    assert killed == [4321]
    assert not sock.exists()
    assert not sock.with_suffix(".pid").exists()
    assert not sock.with_suffix(".session").exists()
