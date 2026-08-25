"""Unit tests for the shared-daemon codex app-server client (pure-logic layer).

The socket transport (`_WSConn`) and live daemon (`spawn_daemon`) need a real
unix socket bind, which is covered by the real-machine smoke, not here. These
tests cover the state mapping, the pane thread records, the spawn/fork mint
protocol, and the config.toml trust writer.
"""
import json
import threading
import time

import pytest

from hive.adapters import codex_app_server as m

pytestmark = pytest.mark.unit


@pytest.fixture(autouse=True)
def _fresh_client_state(monkeypatch):
    """Isolate the module-level shared client between tests."""
    monkeypatch.setattr(m, "_CLIENT", None)
    monkeypatch.setattr(m, "_CLIENT_COOLDOWN_UNTIL", 0.0)


def _bare_client() -> m.CodexDaemonClient:
    """A client without a socket connection, for state-logic tests."""
    c = object.__new__(m.CodexDaemonClient)
    c._state_lock = threading.Lock()
    c._threads = {}
    c._resume_cooldown = {}
    return c


# --- paths & records --------------------------------------------------------


def test_shared_socket_path_under_app_server_control():
    path = m.shared_socket_path()
    assert path.name == "hive-shared.sock"
    assert path.parent.name == "app-server-control"
    # macOS unix socket paths cap at 104 bytes; keep headroom.
    assert len(str(path)) < 104


def test_shared_pidfile_path():
    assert m.shared_pidfile_path().name == "hive-shared.pid"


def test_pane_thread_record_roundtrip(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    m.write_pane_thread("%19", "tid-1", "/work")
    assert m.read_pane_thread("%19") == ("tid-1", "/work")
    assert m.thread_id_for_pane("%19") == "tid-1"
    assert m.session_id_for_pane("%19") == "tid-1"  # threadId == sessionId
    m.clear_pane_thread("%19")
    assert m.read_pane_thread("%19") is None
    m.clear_pane_thread("%19")  # idempotent


def test_read_pane_thread_rejects_garbage(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    path = m.pane_thread_path("%3")
    path.parent.mkdir(parents=True)
    path.write_text("not json")
    assert m.read_pane_thread("%3") is None
    path.write_text(json.dumps({"cwd": "/x"}))  # no threadId
    assert m.read_pane_thread("%3") is None


def test_pane_for_thread_reverse_lookup(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    m.write_pane_thread("%19", "tid-a", "/work")
    m.write_pane_thread("%7", "tid-b", "/work")
    assert m.pane_for_thread("tid-b") == "%7"
    assert m.pane_for_thread("tid-a") == "%19"
    assert m.pane_for_thread("missing") is None
    assert m.pane_for_thread("") is None


def test_list_recorded_panes(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    m.write_pane_thread("%19", "t1", "/w")
    m.write_pane_thread("%7", "t2", "/w")
    (tmp_path / "app-server-control" / "hive-pane-default.thread").write_text("{}")
    assert sorted(m.list_recorded_panes()) == ["%19", "%7"]


def test_list_recorded_panes_missing_dir(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    assert m.list_recorded_panes() == []


def test_daemon_env_strips_pane_identity(monkeypatch):
    # The shared daemon serves every pane: a frozen TMUX_PANE in its env would
    # let untagged tool shells impersonate whichever pane spawned it.
    monkeypatch.setenv("TMUX_PANE", "%old")
    monkeypatch.setenv("HIVE_CODEX_PANE", "%old")
    env = m._daemon_env()
    assert "TMUX_PANE" not in env
    assert "HIVE_CODEX_PANE" not in env


# --- status mapping ---------------------------------------------------------


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


def test_on_notification_ignores_turn_events():
    # turn/* only reaches the turn-owning client on a shared daemon; folding
    # them here would be dead code pretending to be signal.
    c = _bare_client()
    c._on_notification("turn/started", {"threadId": "t1", "turn": {"id": "x"}})
    c._on_notification("turn/completed", {"threadId": "t1"})
    assert c._threads == {}


def test_on_notification_ignores_missing_thread_id():
    c = _bare_client()
    c._on_notification("thread/status/changed", {"status": {"type": "idle"}})
    assert c._threads == {}


def test_runtime_for_returns_copy_not_reference():
    c = _bare_client()
    c._on_notification(
        "thread/status/changed", {"threadId": "t1", "status": {"type": "idle"}}
    )
    snap = c.runtime_for("t1")
    snap.busy = True
    assert not c.runtime_for("t1").busy  # internal state untouched


# --- resume backfill --------------------------------------------------------


def test_resume_backfills_active_runtime_from_thread_status():
    """Late-join recovery: resume must seed _threads from the thread's status
    so runtime reads report native busy/turnPhase instead of None."""
    c = _bare_client()
    c.call = lambda method, params=None, timeout=10.0: {
        "result": {"thread": {"sessionId": "s", "status": {"type": "active", "activeFlags": []}}}
    }
    assert c.resume("t1") is True
    rt = c.runtime_for("t1")
    assert rt is not None and rt.busy


def test_resume_backfills_idle_runtime_from_thread_status():
    c = _bare_client()
    c.call = lambda *a, **k: {
        "result": {"thread": {"sessionId": "s", "status": {"type": "idle"}}}
    }
    assert c.resume("t1") is True
    rt = c.runtime_for("t1")
    assert rt is not None and not rt.busy and rt.turn_phase == "turn_closed"


def test_resume_returns_false_on_error():
    c = _bare_client()
    c.call = lambda *a, **k: {"__error__": "no rollout found"}
    assert c.resume("t1") is False
    assert c._threads == {}


def test_attach_resumes_each_loaded_thread():
    c = _bare_client()
    c.loaded_list = lambda: ["t1", "t2"]
    seen: list[str] = []
    c.resume = lambda tid: seen.append(tid) or True
    c.attach()
    assert seen == ["t1", "t2"]


def test_runtime_or_backfill_resumes_once_per_cooldown():
    c = _bare_client()
    resumes: list[str] = []

    def fake_resume(tid):
        resumes.append(tid)
        return False  # keep the runtime missing

    c.resume = fake_resume
    assert c.runtime_or_backfill("t1") is None
    assert c.runtime_or_backfill("t1") is None  # inside cooldown: no 2nd resume
    assert resumes == ["t1"]


def test_runtime_or_backfill_returns_backfilled_state():
    c = _bare_client()
    c.call = lambda *a, **k: {
        "result": {"thread": {"status": {"type": "idle"}}}
    }
    rt = c.runtime_or_backfill("t1")
    assert rt is not None and rt.turn_phase == "turn_closed"


# --- mint / fork protocol ---------------------------------------------------


def test_start_thread_mints_and_flushes():
    c = _bare_client()
    calls: list[tuple[str, dict]] = []

    def fake_call(method, params=None, timeout=10.0):
        calls.append((method, params or {}))
        if method == "thread/start":
            return {"result": {"thread": {"id": "tid-new", "status": {"type": "idle"}}}}
        return {"result": {}}

    c.call = fake_call
    assert c.start_thread("/work", name="honey.val", model="gpt-x") == "tid-new"
    assert calls[0] == ("thread/start", {"cwd": "/work", "model": "gpt-x"})
    # name/set is the rollout flush: without it the TUI's `codex resume <tid>`
    # fails with `no rollout found` (0.149.0 real-machine verified).
    assert calls[1] == ("thread/name/set", {"threadId": "tid-new", "name": "honey.val"})
    # the mint seeds the runtime so a fresh member reads idle, not unknown
    assert c.runtime_for("tid-new") is not None


def test_start_thread_without_model_omits_param():
    c = _bare_client()
    seen: dict = {}

    def fake_call(method, params=None, timeout=10.0):
        if method == "thread/start":
            seen.update(params or {})
            return {"result": {"thread": {"id": "t"}}}
        return {"result": {}}

    c.call = fake_call
    assert c.start_thread("/work", name="n") == "t"
    assert "model" not in seen


def test_start_thread_fails_when_flush_fails():
    # An unflushed thread is not attachable by the TUI; minting must not
    # report success for a thread `codex resume` would refuse.
    c = _bare_client()
    c.call = lambda method, params=None, timeout=10.0: (
        {"result": {"thread": {"id": "t"}}} if method == "thread/start"
        else {"__error__": "boom"}
    )
    assert c.start_thread("/work", name="n") is None


def test_start_thread_fails_on_rpc_error():
    c = _bare_client()
    c.call = lambda *a, **k: {"__error__": "nope"}
    assert c.start_thread("/work", name="n") is None


def test_fork_thread_returns_fork_id_and_flushes():
    c = _bare_client()
    calls: list[tuple[str, dict]] = []

    def fake_call(method, params=None, timeout=10.0):
        calls.append((method, params or {}))
        if method == "thread/fork":
            return {"result": {"thread": {"id": "tid-fork", "forkedFromId": "tid-src"}}}
        return {"result": {}}

    c.call = fake_call
    assert c.fork_thread("tid-src", name="clone") == "tid-fork"
    assert calls[0] == ("thread/fork", {"threadId": "tid-src"})
    assert calls[1] == ("thread/name/set", {"threadId": "tid-fork", "name": "clone"})


def test_fork_thread_fails_on_rpc_error():
    c = _bare_client()
    c.call = lambda *a, **k: {"__error__": "no rollout found"}
    assert c.fork_thread("tid-src", name="clone") is None


# --- pane-keyed API over the shared client ----------------------------------


def _record(monkeypatch, tmp_path, pane="%1", tid="t1"):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    m.write_pane_thread(pane, tid, "/work")


def test_send_to_pane_turn_starts_even_when_busy(monkeypatch, tmp_path):
    # Busy is not bounced to the composer: turn/start carries steer semantics
    # in core, so hive hands a busy thread straight to the RPC. FakeClient
    # deliberately omits runtime methods: send_to_pane must not consult them.
    _record(monkeypatch, tmp_path)
    sent = []

    class FakeClient:
        def turn_start(self, tid, text):
            sent.append((tid, text))
            return {"result": {}}

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    assert m.send_to_pane("%1", "hi") == m.TURN_START_ACCEPTED
    assert sent == [("t1", "hi")]


def test_send_to_pane_fails_without_record(monkeypatch, tmp_path):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    monkeypatch.setattr(
        m, "_shared_client",
        lambda: pytest.fail("no record -> the daemon must not even be dialed"),
    )
    assert m.send_to_pane("%1", "hi") is None


def test_send_to_pane_fails_without_daemon(monkeypatch, tmp_path):
    _record(monkeypatch, tmp_path)
    monkeypatch.setattr(m, "_shared_client", lambda: None)
    assert m.send_to_pane("%1", "hi") is None


def test_send_to_pane_fails_on_rpc_error_response(monkeypatch, tmp_path):
    _record(monkeypatch, tmp_path)

    class FakeClient:
        def turn_start(self, tid, text):
            return {"error": {"code": -1, "message": "boom"}}

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    assert m.send_to_pane("%1", "hi") is None


def test_send_to_pane_fails_on_rpc_exception(monkeypatch, tmp_path):
    _record(monkeypatch, tmp_path)

    class FakeClient:
        def turn_start(self, tid, text):
            raise OSError("socket reset")

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    assert m.send_to_pane("%1", "hi") is None


def test_runtime_for_pane_reads_recorded_thread(monkeypatch, tmp_path):
    _record(monkeypatch, tmp_path)

    class FakeClient:
        def runtime_or_backfill(self, tid):
            assert tid == "t1"
            return m.ThreadRuntime(busy=True)

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    rt = m.runtime_for_pane("%1")
    assert rt is not None and rt.busy


def test_runtime_for_pane_none_without_record(monkeypatch, tmp_path):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    monkeypatch.setattr(
        m, "_shared_client",
        lambda: pytest.fail("no record -> no daemon dial"),
    )
    assert m.runtime_for_pane("%1") is None


def test_compact_pane_compacts_when_idle(monkeypatch, tmp_path):
    _record(monkeypatch, tmp_path)
    started = []

    class FakeClient:
        def runtime_or_backfill(self, _tid):
            return m.ThreadRuntime(busy=False)

        def compact_start(self, tid):
            started.append(tid)
            return {"result": {}}

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    assert m.compact_pane("%1") == "compacted"
    assert started == ["t1"]


def test_compact_pane_busy_defers_without_aborting_turn(monkeypatch, tmp_path):
    # A Compact turn aborts any running turn, so a busy agent must never be
    # compacted out from under its in-flight work.
    _record(monkeypatch, tmp_path)

    class FakeClient:
        def runtime_or_backfill(self, _tid):
            return m.ThreadRuntime(busy=True)

        def compact_start(self, *_a):
            raise AssertionError("must not compact a busy agent (would abort its turn)")

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    assert m.compact_pane("%1") == "busy"


def test_compact_pane_unavailable_without_record(monkeypatch, tmp_path):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    assert m.compact_pane("%1") == "unavailable"


def test_connect_true_when_client_established(monkeypatch):
    monkeypatch.setattr(m, "_shared_client", lambda: object())
    assert m.connect() is True


def test_connect_false_when_no_daemon(monkeypatch):
    monkeypatch.setattr(m, "_shared_client", lambda: None)
    assert m.connect() is False


def test_start_member_thread_delegates_to_client(monkeypatch):
    class FakeClient:
        def start_thread(self, cwd, *, name, model=""):
            return "tid-x" if (cwd, name, model) == ("/w", "n", "m") else None

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    assert m.start_member_thread("/w", name="n", model="m") == "tid-x"
    monkeypatch.setattr(m, "_shared_client", lambda: None)
    assert m.start_member_thread("/w", name="n") is None


def test_fork_member_thread_delegates_to_client(monkeypatch):
    class FakeClient:
        def fork_thread(self, tid, *, name):
            return "tid-f" if (tid, name) == ("src", "n") else None

    monkeypatch.setattr(m, "_shared_client", lambda: FakeClient())
    assert m.fork_member_thread("src", name="n") == "tid-f"
    monkeypatch.setattr(m, "_shared_client", lambda: None)
    assert m.fork_member_thread("src", name="n") is None


# --- directory trust --------------------------------------------------------


def test_ensure_dir_trusted_creates_config(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    m.ensure_dir_trusted("/work/dir")
    text = (tmp_path / "config.toml").read_text()
    assert '[projects."/work/dir"]' in text
    assert 'trust_level = "trusted"' in text


def test_ensure_dir_trusted_appends_to_existing_config(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    config.write_text('model = "gpt-x"\n')
    m.ensure_dir_trusted("/work/dir")
    text = config.read_text()
    assert text.startswith('model = "gpt-x"\n')
    assert '[projects."/work/dir"]\ntrust_level = "trusted"' in text


def test_ensure_dir_trusted_idempotent(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    m.ensure_dir_trusted("/work/dir")
    first = config.read_text()
    before = config.stat().st_mtime_ns
    m.ensure_dir_trusted("/work/dir")
    assert config.read_text() == first
    assert config.stat().st_mtime_ns == before  # no rewrite on no-op


def test_ensure_dir_trusted_upgrades_existing_entry(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    config.write_text(
        '[projects."/work/dir"]\ntrust_level = "untrusted"\n\n[other]\nk = 1\n'
    )
    m.ensure_dir_trusted("/work/dir")
    text = config.read_text()
    assert 'trust_level = "trusted"' in text
    assert 'trust_level = "untrusted"' not in text
    assert text.count('[projects."/work/dir"]') == 1  # no duplicate table
    assert "[other]" in text


def test_ensure_dir_trusted_adds_missing_key_to_existing_section(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    config.write_text('[projects."/work/dir"]\nother = 1\n')
    m.ensure_dir_trusted("/work/dir")
    text = config.read_text()
    assert text.count('[projects."/work/dir"]') == 1
    assert 'trust_level = "trusted"' in text
    assert "other = 1" in text


def test_ensure_dir_trusted_escapes_quotes(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    m.ensure_dir_trusted('/work/we"ird')
    text = (tmp_path / "config.toml").read_text()
    assert '[projects."/work/we\\"ird"]' in text


def test_ensure_dir_trusted_matches_literal_string_header(tmp_path, monkeypatch):
    # A hand-edited literal-string header must not gain a duplicate table —
    # duplicate tables make the whole config.toml unparsable.
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    config = tmp_path / "config.toml"
    config.write_text("[projects.'/work/dir']\ntrust_level = \"trusted\"\n")
    m.ensure_dir_trusted("/work/dir")
    text = config.read_text()
    assert text.count("/work/dir") == 1
