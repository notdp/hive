import pytest

from hive import sidecar


@pytest.fixture
def reap_env(monkeypatch, tmp_path):
    """One daemon pane on disk; records emit/kill call order."""
    calls: list[tuple] = []
    pidfile = tmp_path / "hive-pane-4.pid"

    monkeypatch.setattr(
        "hive.adapters.codex_app_server.list_daemon_panes", lambda: ["%4"]
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_pidfile_path", lambda pane: pidfile
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.kill_pane_daemon",
        lambda pane: calls.append(("kill", pane)),
    )
    monkeypatch.setattr(
        "hive.notify_debug.emit",
        lambda workspace, event, **fields: calls.append(("emit", workspace, event, fields)),
    )
    return calls, pidfile


def test_cleanup_skips_live_pane(monkeypatch, reap_env):
    calls, _ = reap_env
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: True)

    sidecar._cleanup_dead_codex_daemons("/tmp/ws")

    assert calls == []


def test_cleanup_reaps_dead_pane_and_logs_before_kill(monkeypatch, reap_env):
    calls, pidfile = reap_env
    pidfile.write_text("12345")
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: False)

    sidecar._cleanup_dead_codex_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"pane": "%4", "pid": 12345}),
        ("kill", "%4"),
    ]


def test_cleanup_logs_reap_without_readable_pidfile(monkeypatch, reap_env):
    calls, _ = reap_env
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: False)

    sidecar._cleanup_dead_codex_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"pane": "%4", "pid": None}),
        ("kill", "%4"),
    ]
