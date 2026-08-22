from __future__ import annotations

from hive import identity


def test_bind_and_lookup_session_and_thread(tmp_path, monkeypatch):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path))
    bound = identity.bind(identity.Binding(
        pane_id="%19",
        cli="grok",
        session_id="sess-1",
        thread_id="thr-1",
        pid=4242,
    ))
    assert bound.pane_id == "%19"
    assert identity.lookup_session("sess-1").pane_id == "%19"
    assert identity.lookup_thread("thr-1").cli == "grok"
    assert identity.lookup_pid(4242).session_id == "sess-1"


def test_pane_from_caller_prefers_thread_env(tmp_path, monkeypatch):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path))
    identity.bind(identity.Binding(pane_id="%7", cli="codex", thread_id="thr-x"))
    monkeypatch.setenv("CODEX_THREAD_ID", "thr-x")
    monkeypatch.delenv("GROK_SESSION_ID", raising=False)
    assert identity.pane_from_caller() == "%7"


def test_pane_from_caller_uses_session_env(tmp_path, monkeypatch):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path))
    identity.bind(identity.Binding(pane_id="%3", cli="grok", session_id="abc"))
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.setenv("GROK_SESSION_ID", "abc")
    assert identity.pane_from_caller() == "%3"


def test_get_current_pane_id_prefers_identity_over_tmux(tmp_path, monkeypatch):
    from hive import tmux

    monkeypatch.setenv("HIVE_HOME", str(tmp_path))
    identity.bind(identity.Binding(pane_id="%11", cli="grok", session_id="s11"))
    monkeypatch.setenv("GROK_SESSION_ID", "s11")
    monkeypatch.setenv("TMUX_PANE", "%99")
    assert tmux.get_current_pane_id() == "%11"


def test_get_current_pane_id_falls_back_to_real_tmux(tmp_path, monkeypatch):
    from hive import tmux

    monkeypatch.setenv("HIVE_HOME", str(tmp_path))
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.delenv("GROK_SESSION_ID", raising=False)
    monkeypatch.delenv("HIVE_SESSION_ID", raising=False)
    monkeypatch.setenv("TMUX_PANE", "%42")
    assert tmux.get_current_pane_id() == "%42"
