from __future__ import annotations

from hive import adapters


def test_resolve_current_session_id_from_daemon(monkeypatch):
    """Daemon-backed codex: the session id comes from the app-server daemon."""
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane",
        lambda pane: "sess-daemon" if pane == "%5" else None,
    )

    assert adapters.get("codex").resolve_current_session_id("%5") == "sess-daemon"


def test_resolve_current_session_id_none_without_daemon(monkeypatch):
    """Embedded codex (no daemon socket) is deliberately unsupported: with no
    daemon to ask, the resolver reports no session instead of guessing via the
    pane's tty processes."""
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane",
        lambda _pane: None,  # no daemon to ask
    )

    assert adapters.get("codex").resolve_current_session_id("%9") is None
