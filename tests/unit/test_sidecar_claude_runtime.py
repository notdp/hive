"""Sidecar claude runtime: three-tier bg-job liveness and the supervisor tick."""
from types import SimpleNamespace

import pytest

import hive.sidecar as sidecar
from hive.adapters.claude_bg import EngineSession

pytestmark = pytest.mark.unit


def _engine(status="idle", *, waiting_for="", session_id="sess-live"):
    import time

    return EngineSession(
        pid=4242, job_id="cafe1234", session_id=session_id,
        socket_path="/tmp/cc-socks/4242.sock", cwd="/w",
        status=status, waiting_for=waiting_for, status_updated_at=time.time(),
    )


@pytest.fixture(autouse=True)
def _fresh_jobs_cache(monkeypatch):
    monkeypatch.setattr(sidecar, "_CLAUDE_JOBS_CACHE", None)


def _pin(monkeypatch, *, record, engine, rows):
    monkeypatch.setattr(
        "hive.adapters.claude_bg.read_pane_job", lambda _p: record
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.engine_session_for_job", lambda _j: engine
    )
    monkeypatch.setattr("hive.adapters.claude_bg.list_jobs", lambda **_kw: rows)


# --- _claude_bg_runtime three-tier liveness ----------------------------------


def test_bg_runtime_live_engine_reports_status_and_session(monkeypatch):
    _pin(monkeypatch, record=("cafe1234", "sess-old", "/w"),
         engine=_engine("busy"), rows=[])

    rt = sidecar._claude_bg_runtime("%1")

    assert rt["cliAlive"] is True
    assert rt["busy"] is True
    assert rt["inputState"] == "ready"
    assert rt["sessionId"] == "sess-live"  # engine truth beats the record
    assert rt["_runtimeSource"] == "claude_bg"


def test_bg_runtime_waiting_engine_maps_waiting_for(monkeypatch):
    _pin(monkeypatch, record=("cafe1234", "", "/w"),
         engine=_engine("waiting", waiting_for="input needed"), rows=[])

    rt = sidecar._claude_bg_runtime("%1")

    assert rt["busy"] is False
    assert rt["inputState"] == "waiting_user"
    assert rt["inputReason"] == "registry:input needed"


def test_bg_runtime_asleep_is_reachable_not_dead(monkeypatch):
    # supervisor parked the engine: the ledger row survives without pid/status
    _pin(monkeypatch, record=("cafe1234", "sess-old", "/w"), engine=None,
         rows=[{"id": "cafe1234", "state": "stopped", "sessionId": "sess-row"}])

    rt = sidecar._claude_bg_runtime("%1")

    assert rt["cliAlive"] is True  # asleep, wake-on-delivery — never reaped
    assert rt["busy"] is False
    assert rt["inputState"] == "ready"
    assert rt["_engineState"] == "asleep"
    assert rt["sessionId"] == "sess-row"


def test_bg_runtime_gone_job_is_offline(monkeypatch):
    _pin(monkeypatch, record=("cafe1234", "sess-old", "/w"), engine=None, rows=[])

    rt = sidecar._claude_bg_runtime("%1")

    assert rt["cliAlive"] is False
    assert rt["inputState"] == "offline"
    assert rt["inputReason"] == "engine_gone"
    assert rt["sessionId"] == "sess-old"


def test_bg_runtime_ledger_failure_is_unknown_not_dead(monkeypatch):
    _pin(monkeypatch, record=("cafe1234", "", "/w"), engine=None, rows=None)

    rt = sidecar._claude_bg_runtime("%1")

    assert rt["cliAlive"] is True  # benefit of the doubt: never a reap signal
    assert rt["inputState"] == "unknown"
    assert rt["inputReason"] == "ledger_unavailable"


def test_bg_runtime_none_for_unmanaged_pane(monkeypatch):
    _pin(monkeypatch, record=None, engine=None, rows=[])
    assert sidecar._claude_bg_runtime("%1") is None


def test_jobs_ledger_is_cached_between_reads(monkeypatch):
    calls: list[int] = []
    _pin(monkeypatch, record=("cafe1234", "", "/w"), engine=None, rows=[])
    monkeypatch.setattr(
        "hive.adapters.claude_bg.list_jobs",
        lambda **_kw: calls.append(1) or [],
    )

    sidecar._claude_bg_runtime("%1")
    sidecar._claude_bg_runtime("%1")

    assert len(calls) == 1  # the ~270ms CLI call never runs per tick per pane


# --- agent runtime payload wiring --------------------------------------------


def test_agent_runtime_payload_reaches_bg_branch_without_a_viewer(monkeypatch):
    # viewer gap: no process on the tty, but the pane records a live job —
    # the member must not read as cli_exited
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _p: True)
    monkeypatch.setattr(sidecar, "_busy_output_payload", lambda _p: {"busy": False})
    monkeypatch.setattr(sidecar, "detect_cli_process_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_a, **_k: "")
    _pin(monkeypatch, record=("cafe1234", "", "/w"), engine=_engine("idle"), rows=[])

    rt = sidecar._agent_runtime_payload("%1")

    assert rt["_cli"] == "claude"
    assert rt["cliAlive"] is True
    assert rt["busy"] is False
    assert rt["inputState"] == "ready"
    assert rt["sessionId"] == "sess-live"


def test_claude_registry_busy_prefers_job_engine(monkeypatch):
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: "cafe1234")
    monkeypatch.setattr(
        "hive.adapters.claude_bg.engine_session_for_job", lambda _j: _engine("busy")
    )
    assert sidecar._claude_registry_busy("%1") is True


def test_claude_registry_busy_falls_back_to_interactive_entry(monkeypatch):
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _p: 777)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.session_status",
        lambda pid: ("busy", "") if pid == 777 else None,
    )
    assert sidecar._claude_registry_busy("%1") is True


def test_claude_registry_busy_none_without_any_source(monkeypatch):
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _p: None)
    assert sidecar._claude_registry_busy("%1") is None


# --- supervisor tick: prune records, park orphans -----------------------------


def test_claude_supervisor_tick_parks_jobs_of_dead_panes(monkeypatch):
    monkeypatch.setattr(
        "hive.tmux.list_panes_all",
        lambda: [SimpleNamespace(pane_id="%1")],
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.list_recorded_panes", lambda: ["%1", "%9"]
    )
    records = {"%9": ("dead0001", "s", "/w"), "%1": ("live0001", "s", "/w")}
    monkeypatch.setattr(
        "hive.adapters.claude_bg.read_pane_job", lambda p: records.get(p)
    )
    cleared: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.clear_pane_job", cleared.append
    )
    stopped: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_bg.stop_job", lambda jid, **_kw: stopped.append(jid)
    )

    sidecar._claude_supervisor_tick("/tmp/ws")

    assert cleared == ["%9"]  # the live pane's record is untouched
    assert stopped == ["dead0001"]


def test_claude_supervisor_tick_treats_empty_listing_as_tmux_failure(monkeypatch):
    monkeypatch.setattr("hive.tmux.list_panes_all", lambda: [])
    cleared: list[str] = []
    monkeypatch.setattr("hive.adapters.claude_bg.clear_pane_job", cleared.append)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.list_recorded_panes", lambda: ["%9"]
    )

    sidecar._claude_supervisor_tick("/tmp/ws")

    assert cleared == []  # unknown is not dead: nothing pruned, nothing parked
