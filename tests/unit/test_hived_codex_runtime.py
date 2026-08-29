"""Unit tests for the codex app-server runtime path in the hived.

Covers the field mapping from a daemon ThreadRuntime to the team-runtime
payload, and the session-id best-effort resolver. The socket/daemon themselves
are mocked — live behavior is the real-machine smoke's job.
"""
import pytest

import hive.hived as hived
from hive.adapters.codex_app_server import ThreadRuntime

pytestmark = pytest.mark.unit


def test_codex_app_server_runtime_maps_fields(monkeypatch):
    rt = ThreadRuntime(busy=True, turn_phase="tool_open", input_state="ready")
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: rt
    )
    out = hived._codex_app_server_runtime("%5")
    assert out["busy"] is True
    assert out["turnPhase"] == "tool_open"
    assert out["inputState"] == "ready"
    assert out["_runtimeSource"] == "codex_app_server"


def test_codex_app_server_runtime_none_without_daemon(monkeypatch):
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: None
    )
    assert hived._codex_app_server_runtime("%5") is None


def test_codex_app_server_runtime_waiting_user(monkeypatch):
    rt = ThreadRuntime(busy=True, turn_phase="tool_open", input_state="waiting_user")
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: rt
    )
    out = hived._codex_app_server_runtime("%5")
    assert out["inputState"] == "waiting_user"
    assert out["inputReason"] == "app_server_active_flag"


def test_doctor_verbose_reports_codex_daemon(monkeypatch, tmp_path):
    from pathlib import Path
    from types import SimpleNamespace

    monkeypatch.setattr(
        "hive.team.Team.load",
        classmethod(lambda _cls, _name, **_kw: SimpleNamespace(
            name="t",
            get=lambda _a: SimpleNamespace(pane_id="%5", is_alive=lambda: True),
            agents={"a": object()},
        )),
    )
    monkeypatch.setattr(
        hived, "_member_runtime_payload",
        lambda pane_id, role: {"alive": True, "_cli": "codex"},
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.shared_socket_path",
        lambda: Path("/x/hive-shared.sock"),
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.daemon_alive", lambda: True)
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.thread_id_for_pane", lambda _p: "tid-5"
    )
    diag = hived._doctor_payload(str(tmp_path), "t", "a", verbose=True)
    assert diag["codexDaemon"] == {
        "socket": "/x/hive-shared.sock",
        "alive": True,
        "threadId": "tid-5",
    }
