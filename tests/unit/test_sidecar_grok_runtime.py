"""Unit tests for the grok leader runtime path in the sidecar.

Covers the field mapping from a leader SessionRuntime to the team-runtime
payload, and the spawn-minted session id the grok branch reports. The leader
daemon itself is mocked — live behavior is the real-machine smoke's job.
"""
from types import SimpleNamespace

import pytest

import hive.sidecar as sidecar
from hive.adapters.grok_leader import SessionRuntime

pytestmark = pytest.mark.unit


def test_grok_leader_runtime_maps_fields(monkeypatch):
    rt = SessionRuntime(busy=True, turn_phase="tool_open", input_state="ready")
    monkeypatch.setattr("hive.adapters.grok_leader.runtime_for_pane", lambda _p: rt)
    out = sidecar._grok_leader_runtime("%5")
    assert out["busy"] is True
    assert out["turnPhase"] == "tool_open"
    assert out["inputState"] == "ready"
    assert out["inputReason"] == ""
    assert out["_runtimeSource"] == "grok-leader"


def test_grok_leader_runtime_none_without_daemon(monkeypatch):
    monkeypatch.setattr("hive.adapters.grok_leader.runtime_for_pane", lambda _p: None)
    assert sidecar._grok_leader_runtime("%5") is None


def test_grok_leader_runtime_defaults_empty_input_state_to_ready(monkeypatch):
    rt = SessionRuntime(busy=True, turn_phase="user_prompt_pending", input_state="")
    monkeypatch.setattr("hive.adapters.grok_leader.runtime_for_pane", lambda _p: rt)
    assert sidecar._grok_leader_runtime("%5")["inputState"] == "ready"


def test_grok_leader_runtime_waiting_user(monkeypatch):
    rt = SessionRuntime(busy=True, turn_phase="tool_open", input_state="waiting_user")
    monkeypatch.setattr("hive.adapters.grok_leader.runtime_for_pane", lambda _p: rt)
    out = sidecar._grok_leader_runtime("%5")
    assert out["inputState"] == "waiting_user"
    assert out["inputReason"] == "leader_permission_request"


def _live_grok_pane(monkeypatch, *, runtime, session_id):
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _p: True)
    monkeypatch.setattr(sidecar, "_busy_output_payload", lambda _p: {"busy": False})
    monkeypatch.setattr(
        sidecar, "detect_cli_process_for_pane", lambda _p: SimpleNamespace(name="grok")
    )
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_a, **_k: "")
    monkeypatch.setattr(
        "hive.adapters.grok_leader.runtime_for_pane", lambda _p: runtime
    )
    monkeypatch.setattr(
        "hive.adapters.grok_leader.session_id_for_pane", lambda _p: session_id
    )


def test_agent_payload_grok_branch_reports_minted_session(monkeypatch):
    _live_grok_pane(
        monkeypatch,
        runtime=SessionRuntime(busy=True, turn_phase="tool_open", input_state="ready"),
        session_id="sid-grok-1",
    )
    rt = sidecar._agent_runtime_payload("%5")
    assert rt["cliAlive"] is True
    assert rt["busy"] is True
    assert rt["turnPhase"] == "tool_open"
    assert rt["_runtimeSource"] == "grok-leader"
    assert rt["sessionId"] == "sid-grok-1"


def test_agent_payload_grok_session_unresolved_without_record(monkeypatch):
    _live_grok_pane(
        monkeypatch,
        runtime=SessionRuntime(busy=False, turn_phase="turn_closed", input_state="ready"),
        session_id=None,
    )
    assert sidecar._agent_runtime_payload("%5")["sessionId"] == "unresolved"


def test_agent_payload_grok_reports_unknown_without_leader_runtime(monkeypatch, tmp_path):
    # No leader state to read, and the transcript gate below only knows the
    # claude/codex record shapes — it reads a pending grok permission request
    # as clear and opens the send gate mid-permission. Never fall into it.
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    _live_grok_pane(monkeypatch, runtime=None, session_id="sid-grok-2")
    monkeypatch.setattr(
        "hive.adapters.base.check_input_gate",
        lambda *_a, **_k: pytest.fail("grok must not reach the transcript gate"),
    )

    rt = sidecar._agent_runtime_payload("%5")
    assert rt["sessionId"] == "sid-grok-2"
    assert rt["inputState"] == "unknown"
    assert rt["inputReason"] == "no_leader_runtime"
    assert "_transcript" not in rt
    assert "_runtimeSource" not in rt


@pytest.mark.parametrize("busy", [True, False])
def test_native_daemon_busy_consults_grok_after_codex(monkeypatch, busy):
    monkeypatch.setattr("hive.adapters.codex_app_server.runtime_for_pane", lambda _p: None)
    monkeypatch.setattr(
        "hive.adapters.grok_leader.runtime_for_pane", lambda _p: SessionRuntime(busy=busy)
    )
    assert sidecar._native_daemon_busy("%5") is busy


def test_native_daemon_busy_none_when_no_daemon_holds_the_pane(monkeypatch):
    monkeypatch.setattr("hive.adapters.codex_app_server.runtime_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.adapters.grok_leader.runtime_for_pane", lambda _p: None)
    assert sidecar._native_daemon_busy("%5") is None
