"""Unit tests for the per-pane codex app-server client (pure-logic layer).

The socket transport (`_WSConn`) and live daemon (`spawn_daemon`) need a real
unix socket bind, which is covered by the real-machine smoke, not here. These
tests cover the state-mapping logic the reader thread drives.
"""
import threading
import time

import pytest

from hive.adapters import codex_app_server as m

pytestmark = pytest.mark.unit


def _bare_client() -> m.CodexDaemonClient:
    """A client without a socket connection, for state-logic tests."""
    c = object.__new__(m.CodexDaemonClient)
    c._state_lock = threading.Lock()
    c._threads = {}
    c._session_ids = {}
    c._resume_cooldown = {}
    return c


def test_pane_socket_path_slugifies_pane_id():
    assert m.pane_socket_path("%19").name == "hive-pane-19.sock"
    assert m.pane_socket_path("%7").name == "hive-pane-7.sock"
    assert m.pane_socket_path("").name == "hive-pane-default.sock"


def test_pane_socket_path_under_app_server_control():
    path = m.pane_socket_path("%1")
    assert path.parent.name == "app-server-control"
    # macOS unix socket paths cap at 104 bytes; keep headroom.
    assert len(str(path)) < 104


def test_apply_status_active_ready():
    rt = m.ThreadRuntime()
    m._apply_status(rt, {"type": "active", "activeFlags": []})
    assert rt.busy
    assert rt.input_state == "ready"
    assert rt.turn_phase == "tool_open"


def test_apply_status_active_waiting_on_user_input():
    rt = m.ThreadRuntime()
    m._apply_status(rt, {"type": "active", "activeFlags": ["waitingOnUserInput"]})
    assert rt.input_state == "waiting_user"


def test_apply_status_active_waiting_on_approval():
    rt = m.ThreadRuntime()
    m._apply_status(rt, {"type": "active", "activeFlags": ["waitingOnApproval"]})
    assert rt.input_state == "waiting_user"


def test_apply_status_idle():
    rt = m.ThreadRuntime(busy=True)
    m._apply_status(rt, {"type": "idle"})
    assert not rt.busy
    assert rt.input_state == "ready"
    assert rt.turn_phase == "turn_closed"


def test_apply_status_unknown_kind_preserves_prior_fields():
    rt = m.ThreadRuntime(busy=True, input_state="ready", turn_phase="tool_open")
    m._apply_status(rt, {"type": "systemError"})
    assert rt.busy
    assert rt.input_state == "ready"
    assert rt.turn_phase == "tool_open"


def test_on_notification_turn_lifecycle():
    c = _bare_client()
    c._on_notification("turn/started", {"threadId": "t1", "turn": {"id": "turn-1"}})
    rt = c.runtime_for("t1")
    assert rt.busy
    assert rt.active_turn_id == "turn-1"
    assert rt.turn_phase == "tool_open"

    c._on_notification("turn/completed", {"threadId": "t1"})
    rt = c.runtime_for("t1")
    assert not rt.busy
    assert rt.active_turn_id is None
    assert rt.input_state == "ready"


def test_on_notification_status_changed():
    c = _bare_client()
    c._on_notification(
        "thread/status/changed",
        {"threadId": "t1", "status": {"type": "active", "activeFlags": []}},
    )
    assert c.runtime_for("t1").busy
    c._on_notification(
        "thread/status/changed", {"threadId": "t1", "status": {"type": "idle"}}
    )
    assert not c.runtime_for("t1").busy


def test_on_notification_token_usage_uses_last_not_total():
    c = _bare_client()
    c._on_notification(
        "thread/tokenUsage/updated",
        {
            "threadId": "t1",
            "tokenUsage": {
                "last": {"totalTokens": 1234},
                "total": {"totalTokens": 999999},
                "modelContextWindow": 200000,
            },
        },
    )
    rt = c.runtime_for("t1")
    assert rt.tokens == 1234  # `last`, not cumulative `total`
    assert rt.window == 200000


def test_on_notification_ignores_missing_thread_id():
    c = _bare_client()
    c._on_notification("turn/started", {"turn": {"id": "x"}})
    assert c._threads == {}


def test_latest_runtime_picks_most_recently_observed():
    c = _bare_client()
    c._on_notification(
        "thread/status/changed", {"threadId": "old", "status": {"type": "idle"}}
    )
    time.sleep(0.01)
    c._on_notification(
        "thread/status/changed",
        {"threadId": "new", "status": {"type": "active", "activeFlags": []}},
    )
    rt = c.latest_runtime()
    assert rt.busy  # `new` is active and most recently observed


def test_latest_runtime_none_when_no_threads():
    assert _bare_client().latest_runtime() is None


def test_latest_thread_id_picks_most_recently_observed():
    c = _bare_client()
    c._on_notification(
        "thread/status/changed", {"threadId": "old", "status": {"type": "idle"}}
    )
    time.sleep(0.01)
    c._on_notification(
        "thread/status/changed",
        {"threadId": "new", "status": {"type": "active", "activeFlags": []}},
    )
    assert c.latest_thread_id() == "new"


def test_latest_thread_id_none_when_no_threads():
    assert _bare_client().latest_thread_id() is None


def test_resume_caches_session_id_from_thread_metadata():
    c = _bare_client()
    c.call = lambda method, params=None, timeout=10.0: {
        "result": {"thread": {"sessionId": "sess-uuid"}}
    }
    assert c.resume("t1") is True
    assert c._session_ids["t1"] == "sess-uuid"


def test_resume_returns_false_on_error():
    c = _bare_client()
    c.call = lambda *a, **k: {"__error__": "no rollout found"}
    assert c.resume("t1") is False
    assert "t1" not in c._session_ids


def test_resume_backfills_active_runtime_from_thread_status():
    """Late-join recovery: resume must seed _threads from the thread's status so
    latest_runtime() reports native busy/turnPhase instead of None (which would
    drop the caller to the transcript path)."""
    c = _bare_client()
    c.call = lambda method, params=None, timeout=10.0: {
        "result": {"thread": {"sessionId": "s", "status": {"type": "active", "activeFlags": []}}}
    }
    assert c.resume("t1") is True
    rt = c.runtime_for("t1")
    assert rt is not None and rt.busy
    assert c.latest_runtime() is not None and c.latest_runtime().busy


def test_resume_backfills_idle_runtime_from_thread_status():
    c = _bare_client()
    c.call = lambda *a, **k: {
        "result": {"thread": {"sessionId": "s", "status": {"type": "idle"}}}
    }
    assert c.resume("t1") is True
    rt = c.runtime_for("t1")
    assert rt is not None and not rt.busy and rt.turn_phase == "turn_closed"


def test_resume_without_status_still_caches_session_id():
    c = _bare_client()
    c.call = lambda *a, **k: {"result": {"thread": {"sessionId": "only-sid"}}}
    assert c.resume("t1") is True
    assert c._session_ids["t1"] == "only-sid"
    assert c.runtime_for("t1") is None  # no status -> no runtime fabricated


def test_attach_resumes_with_turns_included_for_token_replay():
    """attach() is the late-join path; it must resume with excludeTurns=False so
    the daemon replays persisted token usage (the cheap excludeTurns=True path
    skips the replay, leaving context tokens unrecovered)."""
    c = _bare_client()
    c.loaded_list = lambda: ["t1"]
    seen: dict = {}

    def fake_resume(tid, *, exclude_turns=True):
        seen["tid"] = tid
        seen["exclude_turns"] = exclude_turns
        return True

    c.resume = fake_resume
    c.attach()
    assert seen == {"tid": "t1", "exclude_turns": False}


def test_ensure_session_id_resumes_and_caches(monkeypatch):
    c = _bare_client()
    c._on_notification(
        "thread/status/changed",
        {"threadId": "t1", "status": {"type": "active", "activeFlags": []}},
    )
    resume_calls = []

    def fake_call(method, params=None, timeout=10.0):
        resume_calls.append(method)
        return {"result": {"thread": {"sessionId": "sess-1"}}}

    c.call = fake_call
    assert c.ensure_session_id() == "sess-1"
    assert resume_calls == ["thread/resume"]
    # cached: a second lookup does not resume again
    c.call = lambda *a, **k: pytest.fail("should not resume a second time")
    assert c.ensure_session_id() == "sess-1"


def test_ensure_session_id_none_without_thread():
    assert _bare_client().ensure_session_id() is None


def test_pool_send_to_pane_turn_starts_even_when_busy(monkeypatch):
    # Busy is no longer bounced to the composer. turn/start carries steer
    # semantics in core (steer the running turn, or open a fresh one when idle),
    # so hive hands a busy thread straight to the RPC and never consults busy.
    # FakeClient deliberately omits runtime_for: send_to_pane must not call it.
    pool = m.CodexClientPool()
    sent = []

    class FakeClient:
        def latest_thread_id(self):
            return "t1"

        def turn_start(self, tid, text):
            sent.append((tid, text))
            return {"result": {}}

    monkeypatch.setattr(pool, "_client_for", lambda _pane: FakeClient())
    assert pool.send_to_pane("%1", "hi") is True
    assert sent == [("t1", "hi")]


def test_pool_send_to_pane_falls_back_without_daemon(monkeypatch):
    pool = m.CodexClientPool()
    monkeypatch.setattr(pool, "_client_for", lambda _pane: None)
    assert pool.send_to_pane("%1", "hi") is False  # no daemon -> keystroke fallback


def test_pool_compact_pane_compacts_when_idle(monkeypatch):
    pool = m.CodexClientPool()
    started = []

    class FakeClient:
        def latest_thread_id(self):
            return "t1"

        def runtime_for(self, _tid):
            return m.ThreadRuntime(busy=False)

        def compact_start(self, tid):
            started.append(tid)
            return {"result": {}}

    monkeypatch.setattr(pool, "_client_for", lambda _pane: FakeClient())
    assert pool.compact_pane("%1") == "compacted"
    assert started == ["t1"]


def test_pool_compact_pane_busy_defers_without_aborting_turn(monkeypatch):
    # A Compact turn aborts any running turn, so a busy agent must never be
    # compacted out from under its in-flight work.
    pool = m.CodexClientPool()

    class FakeClient:
        def latest_thread_id(self):
            return "t1"

        def runtime_for(self, _tid):
            return m.ThreadRuntime(busy=True)

        def compact_start(self, *_a):
            raise AssertionError("must not compact a busy agent (would abort its turn)")

    monkeypatch.setattr(pool, "_client_for", lambda _pane: FakeClient())
    assert pool.compact_pane("%1") == "busy"


def test_pool_compact_pane_unavailable_without_daemon(monkeypatch):
    pool = m.CodexClientPool()
    monkeypatch.setattr(pool, "_client_for", lambda _pane: None)
    assert pool.compact_pane("%1") == "unavailable"


def test_pool_connect_true_when_client_established(monkeypatch):
    pool = m.CodexClientPool()
    monkeypatch.setattr(pool, "_client_for", lambda _pane: object())
    assert pool.connect("%1") is True


def test_pool_connect_false_when_no_daemon(monkeypatch):
    pool = m.CodexClientPool()
    monkeypatch.setattr(pool, "_client_for", lambda _pane: None)
    assert pool.connect("%1") is False


def test_runtime_for_returns_copy_not_reference():
    c = _bare_client()
    c._on_notification(
        "thread/status/changed", {"threadId": "t1", "status": {"type": "idle"}}
    )
    snap = c.runtime_for("t1")
    snap.busy = True
    assert not c.runtime_for("t1").busy  # internal state untouched


def test_pane_pidfile_path():
    assert m.pane_pidfile_path("%19").name == "hive-pane-19.pid"
    assert m.pane_pidfile_path("%19").parent.name == "app-server-control"


def test_daemon_env_marks_native_pane(monkeypatch):
    monkeypatch.setenv("TMUX_PANE", "%old")
    env = m._daemon_env_for_pane("%19")
    assert env["TMUX_PANE"] == "%19"
    assert env["HIVE_CODEX_PANE"] == "%19"


def test_pane_from_socket_name_roundtrip():
    assert m._pane_from_socket_name("hive-pane-19.sock") == "%19"
    assert m._pane_from_socket_name("hive-pane-default.sock") is None
    assert m._pane_from_socket_name("app-server-control.sock") is None
    assert m._pane_from_socket_name("hive-pane-.sock") is None


def test_list_daemon_panes_filters_to_hive_panes(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    ctrl = tmp_path / "app-server-control"
    ctrl.mkdir()
    (ctrl / "hive-pane-19.sock").touch()
    (ctrl / "hive-pane-7.sock").touch()
    (ctrl / "app-server-control.sock").touch()  # codex's own singleton, ignored
    (ctrl / "hive-pane-default.sock").touch()  # non-pane, ignored
    assert sorted(m.list_daemon_panes()) == ["%19", "%7"]


def test_list_daemon_panes_missing_dir(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    assert m.list_daemon_panes() == []
