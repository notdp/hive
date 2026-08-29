"""Hived claude runtime: three-tier bg-job liveness and the supervisor tick."""
from types import SimpleNamespace

import pytest

import hive.hived as hived
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
    monkeypatch.setattr(hived, "_CLAUDE_JOBS_CACHE", None)


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

    rt = hived._claude_bg_runtime("%1")

    assert rt["cliAlive"] is True
    assert rt["busy"] is True
    assert rt["inputState"] == "ready"
    assert rt["sessionId"] == "sess-live"  # engine truth beats the record
    assert rt["_runtimeSource"] == "claude_bg"


def test_bg_runtime_waiting_engine_maps_waiting_for(monkeypatch):
    _pin(monkeypatch, record=("cafe1234", "", "/w"),
         engine=_engine("waiting", waiting_for="input needed"), rows=[])

    rt = hived._claude_bg_runtime("%1")

    assert rt["busy"] is False
    assert rt["inputState"] == "waiting_user"
    assert rt["inputReason"] == "registry:input needed"


def test_bg_runtime_asleep_is_reachable_not_dead(monkeypatch):
    # supervisor parked the engine: the ledger row survives without pid/status
    _pin(monkeypatch, record=("cafe1234", "sess-old", "/w"), engine=None,
         rows=[{"id": "cafe1234", "state": "stopped", "sessionId": "sess-row"}])

    rt = hived._claude_bg_runtime("%1")

    assert rt["cliAlive"] is True  # asleep, wake-on-delivery — never reaped
    assert rt["busy"] is False
    assert rt["inputState"] == "ready"
    assert rt["_engineState"] == "asleep"
    assert rt["sessionId"] == "sess-row"


def test_bg_runtime_gone_job_is_offline(monkeypatch):
    _pin(monkeypatch, record=("cafe1234", "sess-old", "/w"), engine=None, rows=[])

    rt = hived._claude_bg_runtime("%1")

    assert rt["cliAlive"] is False
    assert rt["inputState"] == "offline"
    assert rt["inputReason"] == "engine_gone"
    assert rt["sessionId"] == "sess-old"


def test_bg_runtime_ledger_failure_is_unknown_not_dead(monkeypatch):
    _pin(monkeypatch, record=("cafe1234", "", "/w"), engine=None, rows=None)

    rt = hived._claude_bg_runtime("%1")

    assert rt["cliAlive"] is True  # benefit of the doubt: never a reap signal
    assert rt["inputState"] == "unknown"
    assert rt["inputReason"] == "ledger_unavailable"


def test_bg_runtime_none_for_unmanaged_pane(monkeypatch):
    _pin(monkeypatch, record=None, engine=None, rows=[])
    assert hived._claude_bg_runtime("%1") is None


def test_jobs_ledger_is_cached_between_reads(monkeypatch):
    calls: list[int] = []
    _pin(monkeypatch, record=("cafe1234", "", "/w"), engine=None, rows=[])
    monkeypatch.setattr(
        "hive.adapters.claude_bg.list_jobs",
        lambda **_kw: calls.append(1) or [],
    )

    hived._claude_bg_runtime("%1")
    hived._claude_bg_runtime("%1")

    assert len(calls) == 1  # the ~270ms CLI call never runs per tick per pane


# --- agent runtime payload wiring --------------------------------------------


def test_agent_runtime_payload_reaches_bg_branch_without_a_viewer(monkeypatch):
    # viewer gap: no process on the tty, but the pane records a live job —
    # the member must not read as cli_exited
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _p: True)
    monkeypatch.setattr(hived, "_busy_output_payload", lambda _p: {"busy": False})
    monkeypatch.setattr(hived, "detect_cli_process_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_a, **_k: "")
    _pin(monkeypatch, record=("cafe1234", "", "/w"), engine=_engine("idle"), rows=[])

    rt = hived._agent_runtime_payload("%1")

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
    assert hived._claude_registry_busy("%1") is True


def test_claude_registry_busy_falls_back_to_interactive_entry(monkeypatch):
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _p: 777)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.session_status",
        lambda pid: ("busy", "") if pid == 777 else None,
    )
    assert hived._claude_registry_busy("%1") is True


def test_claude_registry_busy_none_without_any_source(monkeypatch):
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: None)
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _p: None)
    assert hived._claude_registry_busy("%1") is None


# --- interactive claude: the session registry, not the transcript gate --------


def _interactive_claude_pane(monkeypatch, tmp_path, *, status, transcript=True):
    """A live interactive (non-member) claude on the pane tty: no job record,
    a resolvable session, and *status* as its registry entry's report."""
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _p: True)
    monkeypatch.setattr("hive.tmux.display_value", lambda *_a: "/w")
    monkeypatch.setattr(hived, "_busy_output_payload", lambda _p: {"busy": False})
    monkeypatch.setattr(
        hived, "detect_cli_process_for_pane", lambda _p: SimpleNamespace(name="claude")
    )
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_a, **_k: "")
    monkeypatch.setattr("hive.adapters.claude_bg.read_pane_job", lambda _p: None)
    path = tmp_path / "sess-i.jsonl"
    path.write_text("{}\n")
    monkeypatch.setattr(
        "hive.adapters.get",
        lambda _name: SimpleNamespace(
            resolve_current_session_id=lambda _p: "sess-i",
            find_session_file=lambda _sid, cwd=None: path if transcript else None,
        ),
    )
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda _p: 777)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.session_status",
        lambda pid: status if pid == 777 else None,
    )


def test_interactive_claude_takes_input_state_from_its_registry_entry(monkeypatch, tmp_path):
    # the transcript gate only sees an AskUserQuestion record, so it reads every
    # other wait as clear (and a stale ask as pending) — and the send gate
    # refuses on that verdict. The registry is the authority when it speaks.
    _interactive_claude_pane(monkeypatch, tmp_path, status=("waiting", "input needed"))
    monkeypatch.setattr(
        "hive.adapters.base.check_input_gate",
        lambda *_a, **_k: pytest.fail("the registry answered; the gate must not run"),
    )

    rt = hived._agent_runtime_payload("%7")

    assert rt["inputState"] == "waiting_user"
    assert rt["inputReason"] == "registry:input needed"
    assert rt["busy"] is False
    assert rt["sessionId"] == "sess-i"
    assert rt["_runtimeSource"] == "claude_registry"


@pytest.mark.parametrize("status,expected", [
    ("busy", True),
    ("shell", False),
    ("idle", False),
])
def test_interactive_claude_status_maps_like_the_bg_engine(monkeypatch, tmp_path, status, expected):
    _interactive_claude_pane(monkeypatch, tmp_path, status=(status, ""))
    monkeypatch.setattr(
        "hive.adapters.base.check_input_gate",
        lambda *_a, **_k: pytest.fail("the registry answered; the gate must not run"),
    )

    rt = hived._agent_runtime_payload("%7")

    assert rt["busy"] is expected
    assert rt["inputState"] == "ready"  # `shell` is neither mid-turn nor a wait


def test_interactive_claude_without_a_registry_status_falls_back_to_the_gate(monkeypatch, tmp_path):
    # headless/desktop-hosted sessions report nothing; the transcript gate is
    # still the only answer available for them
    _interactive_claude_pane(monkeypatch, tmp_path, status=None)
    from hive.adapters.base import GateResult

    monkeypatch.setattr(
        "hive.adapters.base.check_input_gate", lambda _path: GateResult("waiting", "")
    )

    rt = hived._agent_runtime_payload("%7")

    assert rt["inputState"] == "waiting_user"
    assert rt["inputReason"] == "ask_pending"
    assert "_runtimeSource" not in rt


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

    hived._claude_supervisor_tick("/tmp/ws")

    assert cleared == ["%9"]  # the live pane's record is untouched
    assert stopped == ["dead0001"]


def test_claude_supervisor_tick_treats_empty_listing_as_tmux_failure(monkeypatch):
    monkeypatch.setattr("hive.tmux.list_panes_all", lambda: [])
    cleared: list[str] = []
    monkeypatch.setattr("hive.adapters.claude_bg.clear_pane_job", cleared.append)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.list_recorded_panes", lambda: ["%9"]
    )

    hived._claude_supervisor_tick("/tmp/ws")

    assert cleared == []  # unknown is not dead: nothing pruned, nothing parked
