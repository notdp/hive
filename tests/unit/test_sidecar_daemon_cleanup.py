import pytest

from hive import sidecar

TRANSPORTS = ("codex_app_server", "grok_leader")


@pytest.fixture(params=TRANSPORTS)
def reap_env(monkeypatch, tmp_path, request):
    """One daemon pane on disk for one transport; records emit/kill call order.

    Every transport is patched, so the scan never reaches a real socket
    directory, and the recorded kill carries its transport: cleanup must reap
    the pane through the daemon that owns it.
    """
    calls: list[tuple] = []
    pidfile = tmp_path / "hive-pane-4.pid"

    class _Pool:
        def __init__(self, transport: str):
            self._transport = transport

        def drop(self, pane: str) -> None:
            calls.append(("drop", self._transport, pane))

    for name in TRANSPORTS:
        module = f"hive.adapters.{name}"
        panes = ["%4"] if name == request.param else []
        monkeypatch.setattr(f"{module}.list_daemon_panes", lambda panes=panes: panes)
        monkeypatch.setattr(f"{module}.pane_pidfile_path", lambda pane: pidfile)
        monkeypatch.setattr(
            f"{module}.kill_pane_daemon",
            lambda pane, name=name: calls.append(("kill", name, pane)),
        )
        monkeypatch.setattr(f"{module}.pool", lambda name=name: _Pool(name))
    monkeypatch.setattr(
        "hive.notify_debug.emit",
        lambda workspace, event, **fields: calls.append(("emit", workspace, event, fields)),
    )
    return calls, pidfile, request.param


def test_cleanup_skips_live_pane(monkeypatch, reap_env):
    calls, _, _ = reap_env
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: True)

    sidecar._cleanup_dead_daemons("/tmp/ws")

    assert calls == []


def test_cleanup_reaps_dead_pane_and_logs_before_kill(monkeypatch, reap_env):
    calls, pidfile, transport = reap_env
    pidfile.write_text("12345")
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: False)

    sidecar._cleanup_dead_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"pane": "%4", "pid": 12345}),
        ("drop", transport, "%4"),  # dropped first so a dying grok stdio client
        ("kill", transport, "%4"),  # cannot auto-spawn a replacement leader
    ]


def test_cleanup_logs_reap_without_readable_pidfile(monkeypatch, reap_env):
    calls, _, transport = reap_env
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: False)

    sidecar._cleanup_dead_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"pane": "%4", "pid": None}),
        ("drop", transport, "%4"),
        ("kill", transport, "%4"),
    ]
