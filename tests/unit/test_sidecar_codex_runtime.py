"""Unit tests for the codex app-server runtime path in the sidecar.

Covers the field mapping from a daemon ThreadRuntime to the team-runtime
payload, and the session-id best-effort resolver. The socket/daemon themselves
are mocked — live behavior is the real-machine smoke's job.
"""
import pytest

import hive.sidecar as sidecar
from hive.adapters.codex_app_server import ThreadRuntime

pytestmark = pytest.mark.unit


def test_codex_app_server_runtime_maps_fields(monkeypatch):
    rt = ThreadRuntime(busy=True, turn_phase="tool_open", input_state="ready")
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: rt
    )
    out = sidecar._codex_app_server_runtime("%5")
    assert out["busy"] is True
    assert out["turnPhase"] == "tool_open"
    assert out["inputState"] == "ready"
    assert out["_runtimeSource"] == "codex_app_server"


def test_codex_app_server_runtime_none_without_daemon(monkeypatch):
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: None
    )
    assert sidecar._codex_app_server_runtime("%5") is None


def test_codex_app_server_runtime_waiting_user(monkeypatch):
    rt = ThreadRuntime(busy=True, turn_phase="tool_open", input_state="waiting_user")
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.runtime_for_pane", lambda _p: rt
    )
    out = sidecar._codex_app_server_runtime("%5")
    assert out["inputState"] == "waiting_user"
    assert out["inputReason"] == "app_server_active_flag"


def test_session_id_best_effort_via_daemon_lsof(monkeypatch):
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", lambda _p: "sess-xyz"
    )
    monkeypatch.setattr(
        sidecar._RUNTIME_SNAPSHOTS, "update_session_id", lambda *a, **k: None
    )
    sid = sidecar._codex_session_id_best_effort("%5", runtime_snapshot=None)
    assert sid == "sess-xyz"


def test_session_id_best_effort_unresolved(monkeypatch):
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", lambda _p: None
    )
    assert (
        sidecar._codex_session_id_best_effort("%5", runtime_snapshot=None)
        == "unresolved"
    )
