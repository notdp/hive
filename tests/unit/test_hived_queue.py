import fcntl
import os
import shutil
import tempfile
import threading
import time
from pathlib import Path

import pytest

import hive.hived as hived


@pytest.fixture
def short_workspace():
    # AF_UNIX sun_path caps near 104 bytes: the hived socket cannot live
    # under pytest's long tmp_path.
    base = "/tmp" if os.path.isdir("/tmp") else tempfile.gettempdir()
    d = Path(tempfile.mkdtemp(prefix="hive-sq-", dir=base))
    yield str(d)
    shutil.rmtree(d, ignore_errors=True)


def _serve_in_background(server, workspace: str, *, timeout: float) -> tuple[threading.Thread, dict]:
    served: dict = {}

    def _serve() -> None:
        served["keep_running"] = hived._serve_requests(
            server=server,
            workspace=workspace,
            team="team-a",
            tmux_window="dev:3",
            tmux_window_id="@99",
            hived_started_at="2026-01-01T00:00:00Z",
            timeout=timeout,
        )

    thread = threading.Thread(target=_serve, daemon=True)
    thread.start()
    return thread, served


def test_serve_requests_answers_a_read_while_a_send_holds_the_transport(monkeypatch, short_workspace):
    # C1: delivery may hold the native transport for ~52s while `hive team`
    # gives up after 2s and reports "no hived". Handlers run off the accept
    # loop so the short read is answered immediately.
    started = threading.Event()
    release = threading.Event()

    def _handle(*, request, **_kwargs):
        if request.get("action") == "send":
            started.set()
            release.wait(10.0)
            return {"ok": True, "slow": True}, True
        return {"ok": True, "fast": True}, True

    monkeypatch.setattr(hived, "_handle_request", _handle)
    workspace = short_workspace
    server = hived._open_server_socket(workspace)
    slow_client = threading.Thread(
        target=lambda: hived._request_hived(workspace, {"action": "send"}, timeout=10.0),
        daemon=True,
    )
    serve_thread, served = _serve_in_background(server, workspace, timeout=2.0)
    try:
        slow_client.start()
        assert started.wait(2.0)

        began = time.monotonic()
        response = hived._request_hived(
            workspace,
            {"action": "team-runtime"},
            timeout=hived.SOCKET_READY_TIMEOUT,
        )
        elapsed = time.monotonic() - began

        assert response == {"ok": True, "fast": True}
        assert elapsed < 1.0
    finally:
        release.set()
        slow_client.join(timeout=5.0)
        serve_thread.join(timeout=5.0)
        server.close()
        hived._cleanup_socket(workspace)

    assert served["keep_running"] is True
    assert hived._requests_in_flight() is False


def test_serve_requests_still_retires_the_loop_on_shutdown(monkeypatch, short_workspace):
    monkeypatch.setattr(hived, "_handle_request", lambda **_kwargs: ({"ok": True}, False))
    workspace = short_workspace
    server = hived._open_server_socket(workspace)
    serve_thread, served = _serve_in_background(server, workspace, timeout=1.0)
    try:
        response = hived._request_hived(workspace, {"action": "shutdown"}, timeout=2.0)
        serve_thread.join(timeout=5.0)

        assert response == {"ok": True}
        assert served["keep_running"] is False
    finally:
        hived._SHUTDOWN.clear()
        server.close()
        hived._cleanup_socket(workspace)


def test_socket_alive_requires_matching_api_version(monkeypatch):
    monkeypatch.setattr(
        hived,
        "request_ping",
        lambda *_args, **_kwargs: {"ok": True},
    )
    assert hived._socket_alive("/tmp/ws") is False

    monkeypatch.setattr(
        hived,
        "request_ping",
        lambda *_args, **_kwargs: {"ok": True, "apiVersion": hived.HIVED_API_VERSION},
    )
    assert hived._socket_alive("/tmp/ws") is True


def test_hived_identity_matches_team_and_ignores_window():
    assert hived._hived_identity_matches(
        {"ok": True, "apiVersion": hived.HIVED_API_VERSION},
        team="team-a",
    ) is False
    assert hived._hived_identity_matches(
        {"ok": True, "apiVersion": hived.HIVED_API_VERSION, "team": "team-b"},
        team="team-a",
    ) is False
    assert hived._hived_identity_matches(
        {
            "ok": True,
            "apiVersion": hived.HIVED_API_VERSION,
            "buildHash": "stale",
            "team": "team-a",
        },
        team="team-a",
    ) is False
    # The window is display, not identity: a moved/killed/recreated window
    # must not bounce a healthy hived.
    assert hived._hived_identity_matches(
        {
            "ok": True,
            "apiVersion": hived.HIVED_API_VERSION,
            "buildHash": hived.HIVED_BUILD_HASH,
            "team": "team-a",
            "tmuxWindowId": "@9",
        },
        team="team-a",
    ) is True
    assert hived._hived_identity_matches(
        {
            "ok": True,
            "apiVersion": hived.HIVED_API_VERSION,
            "buildHash": hived.HIVED_BUILD_HASH,
            "team": "team-a",
        },
        team="team-a",
    ) is True


def test_handle_request_ping_returns_hived_identity():
    response, keep_running = hived._handle_request(
        workspace="/tmp/ws",
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        hived_started_at="2026-04-17T00:00:00Z",
        request={"action": "ping"},
    )

    assert keep_running is True
    assert response == {
        "ok": True,
        "apiVersion": hived.HIVED_API_VERSION,
        "buildHash": hived.HIVED_BUILD_HASH,
        "team": "team-a",
        "tmuxWindow": "dev:3",
        "tmuxWindowId": "@99",
        "hived": {
            "pid": response["hived"]["pid"],
            "started_at": "2026-04-17T00:00:00Z",
            "code_hash": hived.HIVED_BUILD_HASH,
        },
    }


def test_handle_request_connect_codex_brings_2nd_client_online(monkeypatch):
    import hive.adapters.codex_app_server as cas
    connected: list[bool] = []
    monkeypatch.setattr(cas, "connect", lambda: connected.append(True) or True)

    response, keep_running = hived._handle_request(
        workspace="/tmp/ws",
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        hived_started_at="2026-04-17T00:00:00Z",
        request={"action": "connect-codex"},
    )

    assert keep_running is True
    assert response == {"ok": True, "connected": True}
    assert connected == [True]


def test_handle_request_connect_grok_brings_2nd_client_online(monkeypatch):
    import hive.adapters.grok_leader as grok_leader
    connected: list[str] = []
    monkeypatch.setattr(grok_leader, "connect_pane", lambda pane: connected.append(pane) or True)

    response, keep_running = hived._handle_request(
        workspace="/tmp/ws",
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        hived_started_at="2026-04-17T00:00:00Z",
        request={"action": "connect-grok", "pane": "%5"},
    )

    assert keep_running is True
    assert response == {"ok": True, "connected": True}
    assert connected == ["%5"]


def test_start_hived_spawns_fresh_python_process(monkeypatch):
    captured: dict[str, object] = {}
    workspace = "/tmp/ws"

    class _FakeProcess:
        pid = 4321

    def _fake_popen(command, **kwargs):
        captured["command"] = command
        captured["stdin_name"] = getattr(kwargs.get("stdin"), "name", "")
        captured["stdout_name"] = getattr(kwargs.get("stdout"), "name", "")
        captured["stderr_name"] = getattr(kwargs.get("stderr"), "name", "")
        captured["start_new_session"] = kwargs.get("start_new_session")
        captured["close_fds"] = kwargs.get("close_fds")
        return _FakeProcess()

    monkeypatch.setattr(hived.sys, "executable", "/tmp/fake-python")
    monkeypatch.setattr(hived.subprocess, "Popen", _fake_popen)

    pid = hived._start_hived(workspace, "team-a", "dev:3", "@99")

    assert pid == 4321
    assert captured["command"] == [
        "/tmp/fake-python",
        "-m",
        "hive.hived",
        "--hived",
        workspace,
        "team-a",
        "dev:3",
        "@99",
    ]
    assert captured["stdin_name"] == hived.os.devnull
    assert captured["stdout_name"] == hived.os.devnull
    assert captured["stderr_name"] == str(hived.devlog.hived_stderr_path(workspace))
    assert captured["start_new_session"] is True
    assert captured["close_fds"] is True


def test_run_spawned_hived_ignores_sigint_and_runs_loop(monkeypatch):
    captured: dict[str, object] = {}

    def _fake_signal(sig, handler):
        captured["signal"] = (sig, handler)

    def _fake_loop(workspace, team, tmux_window, tmux_window_id):
        captured["loop_args"] = (workspace, team, tmux_window, tmux_window_id)

    monkeypatch.setattr(hived.signal, "signal", _fake_signal)
    monkeypatch.setattr(hived, "_hived_loop", _fake_loop)

    exit_code = hived._run_spawned_hived(["--hived", "/tmp/ws", "team-a", "dev:3", "@99"])

    assert exit_code == 0
    assert captured["signal"] == (hived.signal.SIGINT, hived.signal.SIG_IGN)
    assert captured["loop_args"] == ("/tmp/ws", "team-a", "dev:3", "@99")


def test_stale_disk_build_hash_requires_stable_changed_hash(monkeypatch):
    values = iter(["new-hash", "new-hash"])
    monkeypatch.setattr(hived, "_compute_build_hash", lambda: next(values))
    state: dict[str, object] = {}

    assert hived._stale_disk_build_hash_for_reexec(state, now=10.0) is None
    assert state["candidate_hash"] == "new-hash"
    assert hived._stale_disk_build_hash_for_reexec(state, now=14.9) is None
    assert hived._stale_disk_build_hash_for_reexec(state, now=15.0) == "new-hash"


def test_stale_disk_build_hash_clears_candidate_when_code_matches(monkeypatch):
    state: dict[str, object] = {"candidate_hash": "new-hash"}
    monkeypatch.setattr(hived, "_compute_build_hash", lambda: hived.HIVED_BUILD_HASH)

    assert hived._stale_disk_build_hash_for_reexec(state, now=10.0) is None
    assert "candidate_hash" not in state


def test_try_acquire_reexec_lock_returns_inheritable_lock_fd(tmp_path):
    lock_fd = hived._try_acquire_reexec_lock(str(tmp_path))
    try:
        assert lock_fd is not None
        assert os.get_inheritable(lock_fd) is True
    finally:
        hived._release_reexec_lock_fd(lock_fd)


def test_try_acquire_reexec_lock_returns_none_when_lock_is_busy(tmp_path):
    lock_path = hived._lock_path(str(tmp_path))
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    held_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(held_fd, fcntl.LOCK_EX)
        assert hived._try_acquire_reexec_lock(str(tmp_path)) is None
    finally:
        fcntl.flock(held_fd, fcntl.LOCK_UN)
        os.close(held_fd)


def test_reexec_hived_stops_monitor_closes_socket_and_execs(monkeypatch, tmp_path):
    calls: list[tuple] = []

    class _Server:
        def close(self):
            calls.append(("server.close",))

    class _Monitor:
        def stop(self):
            calls.append(("monitor.stop",))

    def _execv(executable, argv):
        calls.append(("execv", executable, argv, hived.os.environ.get(hived._HIVED_REEXEC_LOCK_ENV)))
        raise SystemExit(0)

    monkeypatch.delenv(hived._HIVED_REEXEC_LOCK_ENV, raising=False)
    monkeypatch.setattr(hived.sys, "executable", "/tmp/fake-python")
    monkeypatch.setattr(hived.os, "execv", _execv)
    monkeypatch.setattr(
        hived,
        "_try_acquire_reexec_lock",
        lambda workspace: calls.append(("lock", workspace)) or 42,
    )
    monkeypatch.setattr(hived, "_release_reexec_lock_fd", lambda fd: calls.append(("release", fd)))
    monkeypatch.setattr(hived, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))

    with pytest.raises(SystemExit):
        hived._reexec_hived(
            workspace=str(tmp_path),
            team="team-a",
            tmux_window="dev:3",
            tmux_window_id="@99",
            server=_Server(),
            busy_monitor=_Monitor(),
        )

    assert calls == [
        ("lock", str(tmp_path)),
        ("monitor.stop",),
        ("server.close",),
        ("cleanup", str(tmp_path)),
        (
            "execv",
            "/tmp/fake-python",
            [
                "/tmp/fake-python",
                "-m",
                "hive.hived",
                "--hived",
                str(tmp_path),
                "team-a",
                "dev:3",
                "@99",
            ],
            "42",
        ),
        ("release", 42),
    ]
    assert hived._HIVED_REEXEC_LOCK_ENV not in hived.os.environ


def test_reexec_hived_skips_when_reexec_lock_is_busy(monkeypatch, tmp_path):
    calls: list[str] = []

    class _Server:
        def close(self):
            calls.append("server.close")

    class _Monitor:
        def stop(self):
            calls.append("monitor.stop")

    monkeypatch.setattr(hived, "_try_acquire_reexec_lock", lambda _workspace: None)
    monkeypatch.setattr(hived.os, "execv", lambda *_args: calls.append("execv"))

    replacement = hived._reexec_hived(
        workspace=str(tmp_path),
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        server=_Server(),
        busy_monitor=_Monitor(),
    )

    assert replacement is None
    assert calls == []


def test_reexec_hived_rebinds_and_keeps_serving_when_execv_fails(monkeypatch, tmp_path):
    # execv failing after the teardown used to punch through the loop and
    # leave the window with no hived *and* no socket.
    calls: list[tuple] = []

    class _Server:
        def close(self):
            calls.append(("server.close",))

    class _Monitor:
        def stop(self):
            calls.append(("monitor.stop",))

        def start(self):
            calls.append(("monitor.start",))

    def _execv(_executable, _argv):
        raise OSError(8, "Exec format error")

    rebound = object()
    monitor = _Monitor()
    monkeypatch.delenv(hived._HIVED_REEXEC_LOCK_ENV, raising=False)
    monkeypatch.setattr(hived.os, "execv", _execv)
    monkeypatch.setattr(hived, "_try_acquire_reexec_lock", lambda _workspace: 42)
    monkeypatch.setattr(hived, "_release_reexec_lock_fd", lambda fd: calls.append(("release", fd)))
    monkeypatch.setattr(hived, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))
    monkeypatch.setattr(
        hived,
        "_open_server_socket",
        lambda workspace: calls.append(("open", workspace)) or rebound,
    )

    replacement = hived._reexec_hived(
        workspace=str(tmp_path),
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        server=_Server(),
        busy_monitor=monitor,
    )

    assert replacement is rebound
    assert ("open", str(tmp_path)) in calls
    assert ("monitor.start",) in calls
    assert hived._OUTPUT_BUSY_MONITOR is monitor
    assert hived._HIVED_REEXEC_LOCK_ENV not in hived.os.environ
    hived._set_output_busy_monitor(None)


def test_cleanup_socket_if_owner_skips_foreign_owner(monkeypatch, tmp_path):
    calls: list[tuple] = []
    hived._write_hived_owner(
        str(tmp_path),
        pid=os.getpid() + 1000,
        started_at="2026-04-28T00:00:00Z",
        token="foreign",
    )
    monkeypatch.setattr(hived, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))

    hived._cleanup_socket_if_owner(str(tmp_path), "mine")

    assert calls == []


def test_hived_loop_retires_orphan_before_idle_tick(monkeypatch, tmp_path):
    calls: list[tuple] = []
    real_write_owner = hived._write_hived_owner

    class _Server:
        def close(self):
            calls.append(("server.close",))

    def _write_then_steal(workspace: str, *, pid: int, started_at: str, token: str) -> None:
        real_write_owner(workspace, pid=pid, started_at=started_at, token=token)
        real_write_owner(workspace, pid=pid + 1, started_at=started_at, token="foreign")

    def _emit(_workspace, event, **kwargs):
        calls.append(("emit", event, kwargs))

    monkeypatch.setattr(hived, "_open_server_socket", lambda workspace: calls.append(("open", workspace)) or _Server())
    monkeypatch.setattr(hived, "_write_hived_owner", _write_then_steal)
    monkeypatch.setattr(hived, "_release_reexec_lock_fd", lambda fd: calls.append(("release", fd)))
    monkeypatch.setattr(hived, "_is_tmux_window_alive", lambda _tmux_window_id: True)
    monkeypatch.setattr(hived, "_stale_disk_build_hash_for_reexec", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(hived, "_serve_requests", lambda **_kwargs: calls.append(("serve",)) or True)
    monkeypatch.setattr(hived, "_idle_notify_tick", lambda **_kwargs: calls.append(("idle",)))
    monkeypatch.setattr(hived, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))
    monkeypatch.setattr("hive.notify_debug.emit", _emit)

    hived._hived_loop(str(tmp_path), "team-a", "dev:3", "@99")

    retire_events = [call for call in calls if call[0] == "emit" and call[1] == "hived.retire_orphan"]
    assert retire_events
    assert retire_events[0][2]["currentPid"] == os.getpid()
    assert retire_events[0][2]["socketPid"] == os.getpid() + 1
    assert ("idle",) not in calls
    assert ("serve",) not in calls
    assert ("cleanup", str(tmp_path)) not in calls
    assert ("server.close",) in calls


def test_hived_loop_releases_inherited_reexec_lock_after_socket_ready(monkeypatch, tmp_path):
    calls: list[tuple] = []

    class _Server:
        def close(self):
            calls.append(("server.close",))

    monkeypatch.setenv(hived._HIVED_REEXEC_LOCK_ENV, "77")
    monkeypatch.setattr(hived, "_open_server_socket", lambda workspace: calls.append(("open", workspace)) or _Server())
    monkeypatch.setattr(hived, "_release_reexec_lock_fd", lambda fd: calls.append(("release", fd)))
    monkeypatch.setattr(hived, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))
    monkeypatch.setattr(hived, "_is_tmux_window_alive", lambda _tmux_window_id: False)

    hived._hived_loop(str(tmp_path), "team-a", "", "")

    assert calls == [
        ("open", str(tmp_path)),
        ("release", 77),
        ("release", None),
        ("server.close",),
        ("cleanup", str(tmp_path)),
    ]
    assert hived._HIVED_REEXEC_LOCK_ENV not in hived.os.environ


# --- request budgets (VAL fail-r1 finding 1) ---


def test_send_request_budget_covers_native_submission():
    """The CLI socket budget is strictly longer than the worst-case native
    transport submission: a valid slow acceptance must never surface as
    `hived unavailable`."""
    from hive.adapters import claude_sessions, codex_app_server, grok_leader

    native = max(
        claude_sessions.SUBMIT_TIMEOUT,
        codex_app_server.SUBMIT_TIMEOUT,
        grok_leader.SUBMIT_TIMEOUT,
    )
    assert hived._send_request_timeout() > native


def test_request_send_survives_delayed_but_valid_acceptance(tmp_path, monkeypatch):
    """A hived that answers after a native-budget-scale delay still gets its
    truthful queued response back to the CLI (no duplicate-inviting None)."""
    import os
    import shutil
    import socket as socket_mod
    import tempfile
    import threading
    import time
    from pathlib import Path

    # AF_UNIX sun_path caps at ~104 bytes: the socket cannot live under
    # pytest's long tmp_path, so use a short throwaway dir like production.
    base = "/tmp" if os.path.isdir("/tmp") else tempfile.gettempdir()
    run_dir = Path(tempfile.mkdtemp(prefix="hsq", dir=base))
    workspace = tmp_path / "ws"
    monkeypatch.setattr(hived, "_run_dir", lambda _ws: run_dir)

    # shrink every budget component so the test runs in <1s while keeping the
    # invariant shape: delay < derived budget
    monkeypatch.setattr("hive.adapters.claude_sessions.SUBMIT_TIMEOUT", 0.6)
    monkeypatch.setattr("hive.adapters.codex_app_server.SUBMIT_TIMEOUT", 0.1)
    monkeypatch.setattr("hive.adapters.grok_leader.SUBMIT_TIMEOUT", 0.2)
    monkeypatch.setattr(hived, "REQUEST_SLACK", 0.5)

    srv = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
    srv.bind(str(run_dir / "hived.sock"))
    srv.listen(1)

    def _slow_reply():
        conn, _ = srv.accept()
        with conn:
            while conn.recv(65536):
                pass  # drain request until client half-closes
            time.sleep(0.8)  # valid latency: above the native leg alone, below the derived budget (0.6 + 0.5)
            conn.sendall(b'{"ok": true, "msgId": "x1", "delivery": "queued"}\n')

    threading.Thread(target=_slow_reply, daemon=True).start()
    try:
        response = hived.request_send(
            str(workspace),
            team="t",
            sender_agent="a",
            sender_pane="%1",
            target_agent="b",
            body="hello",
        )
    finally:
        srv.close()
        shutil.rmtree(run_dir, ignore_errors=True)

    assert response is not None
    assert response["delivery"] == "queued"
