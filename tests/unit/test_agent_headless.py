"""Pane-less members: engine-addressed send / interrupt / liveness."""
import pytest

from hive.agent import Agent, DeliveryError

pytestmark = pytest.mark.unit


def _member(cli, session_id="sid-1"):
    return Agent(
        name="rex", team_name="honey", pane_id="", cli=cli, session_id=session_id, cwd="/repo"
    )


def test_headless_codex_send_routes_by_thread(monkeypatch):
    sent = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.send_to_thread",
        lambda tid, text: sent.append((tid, text)) or "turnStartAccepted",
    )
    assert _member("codex").send("hi") == "turnStartAccepted"
    assert sent == [("sid-1", "hi")]


def test_headless_codex_send_without_thread_refuses():
    with pytest.raises(DeliveryError):
        _member("codex", session_id=None).send("hi")


def test_headless_grok_send_routes_by_member_key(monkeypatch):
    sent = []
    monkeypatch.setattr(
        "hive.adapters.grok_leader.send_to_key",
        lambda key, text: sent.append((key, text)) or "sessionPromptQueued",
    )
    assert _member("grok").send("hi") == "sessionPromptQueued"
    assert sent == [("m-honey.rex", "hi")]


def test_headless_claude_send_delivers_to_job(monkeypatch):
    from types import SimpleNamespace

    engine = SimpleNamespace(session_id="sess-9", socket_path="/tmp/x.sock")
    monkeypatch.setattr(
        "hive.adapters.claude_bg.job_row",
        lambda job, **kw: {"id": job} if job == "job-1" else None,
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.engine_session_for_job",
        lambda job: engine if job == "job-1" else None,
    )
    replied = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.daemon_reply",
        lambda sid, text: replied.append((sid, text)) or "udsWriteAccepted",
    )
    assert _member("claude", session_id="job-1").send("hi") == "udsWriteAccepted"
    assert replied == [("sess-9", "hi")]


def test_headless_grok_interrupt_routes_by_member_key(monkeypatch):
    calls = []
    monkeypatch.setattr(
        "hive.adapters.grok_leader.interrupt_key",
        lambda key: calls.append(key) or "sessionCancelSent",
    )
    _member("grok").interrupt()
    assert calls == ["m-honey.rex"]


def test_headless_codex_interrupt_routes_by_thread(monkeypatch):
    calls = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.interrupt_thread",
        lambda tid: calls.append(tid) or "turnInterruptAccepted",
    )
    _member("codex").interrupt()
    assert calls == ["sid-1"]


def test_headless_is_alive_probes_the_engine(monkeypatch):
    monkeypatch.setattr("hive.adapters.codex_app_server.daemon_alive", lambda: True)
    assert _member("codex").is_alive() is True
    monkeypatch.setattr("hive.adapters.codex_app_server.daemon_alive", lambda: False)
    assert _member("codex").is_alive() is False

    monkeypatch.setattr("hive.adapters.grok_leader.probe_socket", lambda p: True)
    assert _member("grok").is_alive() is True

    monkeypatch.setattr("hive.adapters.claude_bg.engine_session_for_job", lambda j: None)
    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda j: {"id": j})
    assert _member("claude", session_id="job-1").is_alive() is True  # asleep is not dead
    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda j: None)
    assert _member("claude", session_id="job-1").is_alive() is False


def test_headless_member_runtime_grok(monkeypatch):
    from types import SimpleNamespace

    from hive import hived

    rt = SimpleNamespace(busy=True, turn_phase="tool_open", input_state="ready")
    monkeypatch.setattr(
        "hive.adapters.grok_leader.runtime_for_key",
        lambda key: rt if key == "m-honey.rex" else None,
    )
    monkeypatch.setattr(
        "hive.adapters.grok_leader.read_session_key",
        lambda key: ("sid-g", "/repo"),
    )
    payload = hived._headless_member_runtime(_member("grok"))
    assert payload["headless"] is True
    assert payload["alive"] is True
    assert payload["busy"] is True
    assert payload["sessionId"] == "sid-g"


def test_headless_member_runtime_unknown_engine():
    from hive import hived

    payload = hived._headless_member_runtime(_member("codex", session_id=None))
    assert payload["alive"] is False
    assert payload["inputState"] == "unknown"


def test_headless_claude_send_falls_back_to_interactive_session(monkeypatch):
    from types import SimpleNamespace

    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda job, **kw: None)
    replied = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.daemon_reply",
        lambda sid, text: replied.append((sid, text)) or "udsWriteAccepted",
    )
    assert _member("claude", session_id="ccd-sid-1").send("hi") == "udsWriteAccepted"
    assert replied == [("ccd-sid-1", "hi")]


def test_headless_claude_session_send_uses_inbox_socket_fallback(monkeypatch):
    from types import SimpleNamespace

    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda job, **kw: None)
    monkeypatch.setattr("hive.adapters.claude_sessions.daemon_reply", lambda sid, text: None)
    live = SimpleNamespace(session_id="ccd-sid-1", socket_path="/tmp/ccd.sock")
    monkeypatch.setattr("hive.adapters.claude_sessions.list_sessions", lambda: [live])
    sent = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender, session_id: sent.append((sock, session_id)) or "accepted",
    )
    assert _member("claude", session_id="ccd-sid-1").send("hi") == "accepted"
    assert sent == [("/tmp/ccd.sock", "ccd-sid-1")]


def test_headless_claude_kill_never_stops_an_interactive_session(monkeypatch):
    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda job, **kw: None)
    stopped = []
    monkeypatch.setattr("hive.adapters.claude_bg.stop_job", lambda job: stopped.append(job))
    _member("claude", session_id="ccd-sid-1").kill()
    assert stopped == []
