import fcntl
import os

import pytest

import hive.sidecar as sidecar


def test_socket_alive_requires_matching_api_version(monkeypatch):
    monkeypatch.setattr(
        sidecar,
        "request_ping",
        lambda *_args, **_kwargs: {"ok": True},
    )
    assert sidecar._socket_alive("/tmp/ws") is False

    monkeypatch.setattr(
        sidecar,
        "request_ping",
        lambda *_args, **_kwargs: {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION},
    )
    assert sidecar._socket_alive("/tmp/ws") is True


def test_sidecar_identity_requires_matching_team_and_window_id():
    assert sidecar._sidecar_identity_matches(
        {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION},
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION, "team": "team-b", "tmuxWindowId": "@7"},
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {"ok": True, "apiVersion": sidecar.SIDECAR_API_VERSION, "team": "team-a", "tmuxWindowId": "@9"},
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {
            "ok": True,
            "apiVersion": sidecar.SIDECAR_API_VERSION,
            "team": "team-a",
            "tmuxWindowId": "@7",
        },
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {
            "ok": True,
            "apiVersion": sidecar.SIDECAR_API_VERSION,
            "buildHash": "stale",
            "team": "team-a",
            "tmuxWindowId": "@7",
        },
        team="team-a",
        tmux_window_id="@7",
    ) is False
    assert sidecar._sidecar_identity_matches(
        {
            "ok": True,
            "apiVersion": sidecar.SIDECAR_API_VERSION,
            "buildHash": sidecar.SIDECAR_BUILD_HASH,
            "team": "team-a",
            "tmuxWindowId": "@7",
        },
        team="team-a",
        tmux_window_id="@7",
    ) is True


def test_handle_request_ping_returns_sidecar_identity():
    response, keep_running = sidecar._handle_request(
        workspace="/tmp/ws",
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        sidecar_started_at="2026-04-17T00:00:00Z",
        request={"action": "ping"},
    )

    assert keep_running is True
    assert response == {
        "ok": True,
        "apiVersion": sidecar.SIDECAR_API_VERSION,
        "buildHash": sidecar.SIDECAR_BUILD_HASH,
        "team": "team-a",
        "tmuxWindow": "dev:3",
        "tmuxWindowId": "@99",
        "sidecar": {
            "pid": response["sidecar"]["pid"],
            "started_at": "2026-04-17T00:00:00Z",
            "code_hash": sidecar.SIDECAR_BUILD_HASH,
        },
    }


def test_handle_request_connect_codex_brings_2nd_client_online(monkeypatch):
    import hive.adapters.codex_app_server as cas
    connected: list[str] = []
    monkeypatch.setattr(cas, "connect_pane", lambda pane: connected.append(pane) or True)

    response, keep_running = sidecar._handle_request(
        workspace="/tmp/ws",
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        sidecar_started_at="2026-04-17T00:00:00Z",
        request={"action": "connect-codex", "pane": "%5"},
    )

    assert keep_running is True
    assert response == {"ok": True, "connected": True}
    assert connected == ["%5"]


def test_start_sidecar_spawns_fresh_python_process(monkeypatch):
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

    monkeypatch.setattr(sidecar.sys, "executable", "/tmp/fake-python")
    monkeypatch.setattr(sidecar.subprocess, "Popen", _fake_popen)

    pid = sidecar._start_sidecar(workspace, "team-a", "dev:3", "@99")

    assert pid == 4321
    assert captured["command"] == [
        "/tmp/fake-python",
        "-m",
        "hive.sidecar",
        "--sidecar",
        workspace,
        "team-a",
        "dev:3",
        "@99",
    ]
    assert captured["stdin_name"] == sidecar.os.devnull
    assert captured["stdout_name"] == sidecar.os.devnull
    assert captured["stderr_name"] == str(sidecar.devlog.sidecar_stderr_path(workspace))
    assert captured["start_new_session"] is True
    assert captured["close_fds"] is True


def test_run_spawned_sidecar_ignores_sigint_and_runs_loop(monkeypatch):
    captured: dict[str, object] = {}

    def _fake_signal(sig, handler):
        captured["signal"] = (sig, handler)

    def _fake_loop(workspace, team, tmux_window, tmux_window_id):
        captured["loop_args"] = (workspace, team, tmux_window, tmux_window_id)

    monkeypatch.setattr(sidecar.signal, "signal", _fake_signal)
    monkeypatch.setattr(sidecar, "_sidecar_loop", _fake_loop)

    exit_code = sidecar._run_spawned_sidecar(["--sidecar", "/tmp/ws", "team-a", "dev:3", "@99"])

    assert exit_code == 0
    assert captured["signal"] == (sidecar.signal.SIGINT, sidecar.signal.SIG_IGN)
    assert captured["loop_args"] == ("/tmp/ws", "team-a", "dev:3", "@99")


def test_stale_disk_build_hash_requires_stable_changed_hash(monkeypatch):
    values = iter(["new-hash", "new-hash"])
    monkeypatch.setattr(sidecar, "_compute_build_hash", lambda: next(values))
    state: dict[str, object] = {}

    assert sidecar._stale_disk_build_hash_for_reexec(state, now=10.0) is None
    assert state["candidate_hash"] == "new-hash"
    assert sidecar._stale_disk_build_hash_for_reexec(state, now=14.9) is None
    assert sidecar._stale_disk_build_hash_for_reexec(state, now=15.0) == "new-hash"


def test_stale_disk_build_hash_clears_candidate_when_code_matches(monkeypatch):
    state: dict[str, object] = {"candidate_hash": "new-hash"}
    monkeypatch.setattr(sidecar, "_compute_build_hash", lambda: sidecar.SIDECAR_BUILD_HASH)

    assert sidecar._stale_disk_build_hash_for_reexec(state, now=10.0) is None
    assert "candidate_hash" not in state


def test_try_acquire_reexec_lock_returns_inheritable_lock_fd(tmp_path):
    lock_fd = sidecar._try_acquire_reexec_lock(str(tmp_path))
    try:
        assert lock_fd is not None
        assert os.get_inheritable(lock_fd) is True
    finally:
        sidecar._release_reexec_lock_fd(lock_fd)


def test_try_acquire_reexec_lock_returns_none_when_lock_is_busy(tmp_path):
    lock_path = sidecar._lock_path(str(tmp_path))
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    held_fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(held_fd, fcntl.LOCK_EX)
        assert sidecar._try_acquire_reexec_lock(str(tmp_path)) is None
    finally:
        fcntl.flock(held_fd, fcntl.LOCK_UN)
        os.close(held_fd)


def test_reexec_sidecar_stops_monitor_closes_socket_and_execs(monkeypatch, tmp_path):
    calls: list[tuple] = []

    class _Server:
        def close(self):
            calls.append(("server.close",))

    class _Monitor:
        def stop(self):
            calls.append(("monitor.stop",))

    def _execv(executable, argv):
        calls.append(("execv", executable, argv, sidecar.os.environ.get(sidecar._SIDECAR_REEXEC_LOCK_ENV)))
        raise SystemExit(0)

    monkeypatch.delenv(sidecar._SIDECAR_REEXEC_LOCK_ENV, raising=False)
    monkeypatch.setattr(sidecar.sys, "executable", "/tmp/fake-python")
    monkeypatch.setattr(sidecar.os, "execv", _execv)
    monkeypatch.setattr(
        sidecar,
        "_try_acquire_reexec_lock",
        lambda workspace: calls.append(("lock", workspace)) or 42,
    )
    monkeypatch.setattr(sidecar, "_release_reexec_lock_fd", lambda fd: calls.append(("release", fd)))
    monkeypatch.setattr(sidecar, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))

    with pytest.raises(SystemExit):
        sidecar._reexec_sidecar(
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
                "hive.sidecar",
                "--sidecar",
                str(tmp_path),
                "team-a",
                "dev:3",
                "@99",
            ],
            "42",
        ),
        ("release", 42),
    ]
    assert sidecar._SIDECAR_REEXEC_LOCK_ENV not in sidecar.os.environ


def test_reexec_sidecar_skips_when_reexec_lock_is_busy(monkeypatch, tmp_path):
    calls: list[str] = []

    class _Server:
        def close(self):
            calls.append("server.close")

    class _Monitor:
        def stop(self):
            calls.append("monitor.stop")

    monkeypatch.setattr(sidecar, "_try_acquire_reexec_lock", lambda _workspace: None)
    monkeypatch.setattr(sidecar.os, "execv", lambda *_args: calls.append("execv"))

    did_reexec = sidecar._reexec_sidecar(
        workspace=str(tmp_path),
        team="team-a",
        tmux_window="dev:3",
        tmux_window_id="@99",
        server=_Server(),
        busy_monitor=_Monitor(),
    )

    assert did_reexec is False
    assert calls == []


def test_cleanup_socket_if_owner_skips_foreign_owner(monkeypatch, tmp_path):
    calls: list[tuple] = []
    sidecar._write_sidecar_owner(
        str(tmp_path),
        pid=os.getpid() + 1000,
        started_at="2026-04-28T00:00:00Z",
        token="foreign",
    )
    monkeypatch.setattr(sidecar, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))

    sidecar._cleanup_socket_if_owner(str(tmp_path), "mine")

    assert calls == []


def test_sidecar_loop_retires_orphan_before_idle_tick(monkeypatch, tmp_path):
    calls: list[tuple] = []
    real_write_owner = sidecar._write_sidecar_owner

    class _Server:
        def close(self):
            calls.append(("server.close",))

    def _write_then_steal(workspace: str, *, pid: int, started_at: str, token: str) -> None:
        real_write_owner(workspace, pid=pid, started_at=started_at, token=token)
        real_write_owner(workspace, pid=pid + 1, started_at=started_at, token="foreign")

    def _emit(_workspace, event, **kwargs):
        calls.append(("emit", event, kwargs))

    monkeypatch.setattr(sidecar, "_open_server_socket", lambda workspace: calls.append(("open", workspace)) or _Server())
    monkeypatch.setattr(sidecar, "_write_sidecar_owner", _write_then_steal)
    monkeypatch.setattr(sidecar, "_release_reexec_lock_fd", lambda fd: calls.append(("release", fd)))
    monkeypatch.setattr(sidecar, "_is_tmux_window_alive", lambda _tmux_window_id: True)
    monkeypatch.setattr(sidecar, "_stale_disk_build_hash_for_reexec", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(sidecar, "_serve_requests", lambda **_kwargs: calls.append(("serve",)) or True)
    monkeypatch.setattr(sidecar, "_idle_notify_tick", lambda **_kwargs: calls.append(("idle",)))
    monkeypatch.setattr(sidecar, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))
    monkeypatch.setattr("hive.notify_debug.emit", _emit)

    sidecar._sidecar_loop(str(tmp_path), "team-a", "dev:3", "@99")

    retire_events = [call for call in calls if call[0] == "emit" and call[1] == "sidecar.retire_orphan"]
    assert retire_events
    assert retire_events[0][2]["currentPid"] == os.getpid()
    assert retire_events[0][2]["socketPid"] == os.getpid() + 1
    assert ("idle",) not in calls
    assert ("serve",) not in calls
    assert ("cleanup", str(tmp_path)) not in calls
    assert ("server.close",) in calls


def test_sidecar_loop_releases_inherited_reexec_lock_after_socket_ready(monkeypatch, tmp_path):
    calls: list[tuple] = []

    class _Server:
        def close(self):
            calls.append(("server.close",))

    monkeypatch.setenv(sidecar._SIDECAR_REEXEC_LOCK_ENV, "77")
    monkeypatch.setattr(sidecar, "_open_server_socket", lambda workspace: calls.append(("open", workspace)) or _Server())
    monkeypatch.setattr(sidecar, "_release_reexec_lock_fd", lambda fd: calls.append(("release", fd)))
    monkeypatch.setattr(sidecar, "_cleanup_socket", lambda workspace: calls.append(("cleanup", workspace)))
    monkeypatch.setattr(sidecar, "_is_tmux_window_alive", lambda _tmux_window_id: False)

    sidecar._sidecar_loop(str(tmp_path), "team-a", "", "")

    assert calls == [
        ("open", str(tmp_path)),
        ("release", 77),
        ("release", None),
        ("server.close",),
        ("cleanup", str(tmp_path)),
    ]
    assert sidecar._SIDECAR_REEXEC_LOCK_ENV not in sidecar.os.environ


# --- request budgets (VAL fail-r1 finding 1) ---


def test_send_request_budget_covers_native_submission():
    """The CLI socket budget is strictly longer than the worst-case native
    transport submission: a valid slow acceptance must never surface as
    `sidecar unavailable`."""
    from hive.adapters import claude_sessions, codex_app_server

    native = max(claude_sessions.SUBMIT_TIMEOUT, codex_app_server.SUBMIT_TIMEOUT)
    assert sidecar._send_request_timeout() > native


def test_request_send_survives_delayed_but_valid_acceptance(tmp_path, monkeypatch):
    """A sidecar that answers after a native-budget-scale delay still gets its
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
    monkeypatch.setattr(sidecar, "_run_dir", lambda _ws: run_dir)

    # shrink every budget component so the test runs in <1s while keeping the
    # invariant shape: delay < derived budget
    monkeypatch.setattr("hive.adapters.claude_sessions.SUBMIT_TIMEOUT", 0.6)
    monkeypatch.setattr("hive.adapters.codex_app_server.SUBMIT_TIMEOUT", 0.1)
    monkeypatch.setattr(sidecar, "REQUEST_SLACK", 0.5)

    srv = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
    srv.bind(str(run_dir / "sidecar.sock"))
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
        response = sidecar.request_send(
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
