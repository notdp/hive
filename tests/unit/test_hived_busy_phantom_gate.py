"""Phantom-redraw gate for the busy/output-activity signal.

Bug A: tmux control-mode reports "visible text" when a TUI agent
(Ink / ratatui) re-prints already-on-screen characters during idle.
Without a gate this triggers idle-notify fire even though no real LLM
activity happened. The gate cross-checks the transcript jsonl mtime:
if the jsonl has not been touched within ``BUSY_OUTPUT_THRESHOLD_SECONDS``
the busy spike is suppressed.

When the transcript path can't be resolved (non-agent pane, no session
yet, stat error), the gate returns ``None`` and callers fall back to the
raw control-mode signal so notify never silently disappears for panes
the gate can't introspect.
"""

import os
import time

import pytest

import hive.hived as hived


class _Monitor:
    def __init__(self, *, busy: bool = False, last_output_age: float | None = None):
        self._busy = busy
        self._last_output_age = last_output_age

    def is_busy(self, pane_id: str, *, threshold_seconds: float) -> bool:
        return self._busy

    def last_output_age(self, pane_id: str) -> float | None:
        return self._last_output_age


@pytest.fixture(autouse=True)
def _reset_path_cache(monkeypatch):
    hived._TRANSCRIPT_PATH_CACHE.clear()
    monkeypatch.setattr(hived, "_native_daemon_busy", lambda _pane_id: None)
    yield
    hived._TRANSCRIPT_PATH_CACHE.clear()


def _stub_path(monkeypatch, path_str: str | None) -> None:
    monkeypatch.setattr(
        hived,
        "_resolve_transcript_path_cached",
        lambda _pane_id, *, force=False: path_str,
    )


def _stub_path_with_force(monkeypatch, *, cached: str | None, fresh: str | None) -> None:
    def _fake(_pane_id, *, force: bool = False) -> str | None:
        return fresh if force else cached

    monkeypatch.setattr(hived, "_resolve_transcript_path_cached", _fake)


def _backdate(path: str, age_seconds: float) -> None:
    when = time.time() - age_seconds
    os.utime(path, (when, when))


# --- _transcript_progressed_recently three-state ----------------------------


def test_progressed_returns_none_when_path_unknown(monkeypatch):
    _stub_path(monkeypatch, None)
    assert hived._transcript_progressed_recently("%1", 3.0) is None


def test_progressed_returns_none_when_stat_fails(monkeypatch, tmp_path):
    ghost = tmp_path / "missing.jsonl"
    _stub_path(monkeypatch, str(ghost))
    assert hived._transcript_progressed_recently("%1", 3.0) is None


def test_progressed_returns_true_when_mtime_fresh(monkeypatch, tmp_path):
    fresh = tmp_path / "fresh.jsonl"
    fresh.write_text("x")
    _stub_path(monkeypatch, str(fresh))
    assert hived._transcript_progressed_recently("%1", 3.0) is True


def test_progressed_returns_false_when_mtime_stale(monkeypatch, tmp_path):
    stale = tmp_path / "stale.jsonl"
    stale.write_text("x")
    _backdate(str(stale), 60.0)
    _stub_path(monkeypatch, str(stale))
    assert hived._transcript_progressed_recently("%1", 3.0) is False


# --- session-switch stale-bypass (Claude /new) ------------------------------


def test_progressed_recovers_from_session_switch(monkeypatch, tmp_path):
    """Cached path stale but a forced re-resolve yields a fresh new-session
    jsonl (e.g. user ran ``/new``). Real activity must not be suppressed."""
    old = tmp_path / "old.jsonl"
    old.write_text("x")
    _backdate(str(old), 60.0)

    new = tmp_path / "new.jsonl"
    new.write_text("y")  # mtime = now

    _stub_path_with_force(monkeypatch, cached=str(old), fresh=str(new))
    assert hived._transcript_progressed_recently("%1", 3.0) is True


def test_progressed_returns_false_when_re_resolve_yields_same_path(monkeypatch, tmp_path):
    """Truly stale: forced re-resolve points at the same path, so it's a
    real phantom redraw, not a session switch."""
    stale = tmp_path / "stale.jsonl"
    stale.write_text("x")
    _backdate(str(stale), 60.0)

    _stub_path_with_force(monkeypatch, cached=str(stale), fresh=str(stale))
    assert hived._transcript_progressed_recently("%1", 3.0) is False


def test_progressed_returns_false_when_new_session_also_stale(monkeypatch, tmp_path):
    """Session switched but the new jsonl hasn't been written yet."""
    old = tmp_path / "old.jsonl"
    old.write_text("x")
    _backdate(str(old), 60.0)
    new = tmp_path / "new.jsonl"
    new.write_text("y")
    _backdate(str(new), 30.0)

    _stub_path_with_force(monkeypatch, cached=str(old), fresh=str(new))
    assert hived._transcript_progressed_recently("%1", 3.0) is False


def test_progressed_returns_false_when_fresh_resolve_yields_no_path(monkeypatch, tmp_path):
    """Stale cache + can't re-resolve at all (path resolution lost)."""
    stale = tmp_path / "stale.jsonl"
    stale.write_text("x")
    _backdate(str(stale), 60.0)

    _stub_path_with_force(monkeypatch, cached=str(stale), fresh=None)
    assert hived._transcript_progressed_recently("%1", 3.0) is False


# --- Native daemon (codex app-server / grok leader) authoritative override ---


def _stub_app_server_busy(monkeypatch, value: bool | None) -> None:
    monkeypatch.setattr(hived, "_native_daemon_busy", lambda _pane_id: value)


def test_truly_busy_true_when_app_server_busy(monkeypatch):
    _stub_path(monkeypatch, None)
    _stub_app_server_busy(monkeypatch, True)
    assert hived._pane_is_truly_busy("%1", _Monitor(busy=False)) is True


def test_truly_busy_false_when_app_server_idle(monkeypatch, tmp_path):
    """App server says idle → authoritative even if tmux monitor reports output."""
    fresh = tmp_path / "fresh.jsonl"
    fresh.write_text("x")
    _stub_path(monkeypatch, str(fresh))
    _stub_app_server_busy(monkeypatch, False)
    assert hived._pane_is_truly_busy("%1", _Monitor(busy=True)) is False


def test_truly_busy_falls_through_when_no_app_server(monkeypatch, tmp_path):
    """No daemon → None → fall through to existing monitor + transcript."""
    fresh = tmp_path / "fresh.jsonl"
    fresh.write_text("x")
    _stub_path(monkeypatch, str(fresh))
    _stub_app_server_busy(monkeypatch, None)
    assert hived._pane_is_truly_busy("%1", _Monitor(busy=True)) is True


def test_is_output_busy_true_when_app_server_busy(monkeypatch):
    _stub_path(monkeypatch, None)
    _stub_app_server_busy(monkeypatch, True)
    assert hived._is_output_busy("%1", _Monitor(busy=False)) is True


def test_is_output_busy_false_when_app_server_idle(monkeypatch):
    """App server idle → authoritative for idle-notify, prevents false rearm."""
    _stub_path(monkeypatch, None)
    _stub_app_server_busy(monkeypatch, False)
    assert hived._is_output_busy("%1", _Monitor(busy=True)) is False


# --- _pane_is_truly_busy ----------------------------------------------------


def test_truly_busy_false_when_monitor_idle(monkeypatch):
    _stub_path(monkeypatch, None)
    assert hived._pane_is_truly_busy("%1", _Monitor(busy=False)) is False


def test_truly_busy_falls_back_to_monitor_when_path_unknown(monkeypatch):
    """Fallback contract: never silently disable notify for panes the gate
    can't introspect (kiki's review point)."""
    _stub_path(monkeypatch, None)
    assert hived._pane_is_truly_busy("%1", _Monitor(busy=True)) is True


def test_truly_busy_true_when_monitor_busy_and_transcript_fresh(monkeypatch, tmp_path):
    fresh = tmp_path / "fresh.jsonl"
    fresh.write_text("x")
    _stub_path(monkeypatch, str(fresh))
    assert hived._pane_is_truly_busy("%1", _Monitor(busy=True)) is True


def test_truly_busy_false_when_monitor_busy_but_transcript_stale(monkeypatch, tmp_path):
    """Production phantom case (team 0-4 at UTC 08:48:02): control-mode
    reports activity but jsonl is 40+ minutes cold."""
    stale = tmp_path / "stale.jsonl"
    stale.write_text("x")
    _backdate(str(stale), 60.0)
    _stub_path(monkeypatch, str(stale))
    assert hived._pane_is_truly_busy("%1", _Monitor(busy=True)) is False


def test_truly_busy_false_when_monitor_none():
    assert hived._pane_is_truly_busy("%1", None) is False


def test_truly_busy_false_when_pane_id_empty(monkeypatch):
    _stub_path(monkeypatch, None)
    assert hived._pane_is_truly_busy("", _Monitor(busy=True)) is False


# --- _is_output_busy keeps inactive_age semantics ---------------------------


def test_is_output_busy_respects_inactive_age_when_truly_busy(monkeypatch, tmp_path):
    fresh = tmp_path / "fresh.jsonl"
    fresh.write_text("x")
    _stub_path(monkeypatch, str(fresh))

    monitor = _Monitor(busy=True, last_output_age=2.0)
    assert hived._is_output_busy("%1", monitor, inactive_age=5.0) is True
    assert hived._is_output_busy("%1", monitor, inactive_age=1.0) is False


def test_is_output_busy_native_busy_bypasses_inactive_age(monkeypatch):
    """A native runtime source saying busy is independent of when the user
    last viewed the window — agent mid-turn is busy regardless of
    inactive_age. Without this bypass, idle-notify fires ~5s after every
    window switch even while the agent is streaming."""
    _stub_path(monkeypatch, None)
    _stub_app_server_busy(monkeypatch, True)
    monitor = _Monitor(busy=False, last_output_age=20.0)
    assert hived._is_output_busy("%1", monitor, inactive_age=5.0) is True


def test_is_output_busy_skips_inactive_age_when_phantom(monkeypatch, tmp_path):
    stale = tmp_path / "stale.jsonl"
    stale.write_text("x")
    _backdate(str(stale), 60.0)
    _stub_path(monkeypatch, str(stale))

    monitor = _Monitor(busy=True, last_output_age=0.5)
    assert hived._is_output_busy("%1", monitor, inactive_age=5.0) is False


# --- _resolve_transcript_path_cached TTL ------------------------------------


def test_path_cache_hits_within_ttl(monkeypatch):
    calls: list[str] = []

    def _fake_alive(pane_id):
        calls.append(pane_id)
        return False

    monkeypatch.setattr("hive.tmux.is_pane_alive", _fake_alive)

    assert hived._resolve_transcript_path_cached("%1") is None
    assert hived._resolve_transcript_path_cached("%1") is None
    assert calls == ["%1"]


def test_path_cache_refreshes_after_ttl(monkeypatch):
    calls: list[str] = []

    def _fake_alive(pane_id):
        calls.append(pane_id)
        return False

    monkeypatch.setattr("hive.tmux.is_pane_alive", _fake_alive)

    hived._resolve_transcript_path_cached("%1")
    assert len(calls) == 1

    hived._TRANSCRIPT_PATH_CACHE["%1"] = ("", time.monotonic() - 1.0, "")
    hived._resolve_transcript_path_cached("%1")
    assert len(calls) == 2
