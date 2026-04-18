"""Tests for hive delivery and doctor commands."""

import json

from hive import bus
from hive.cli import cli
import hive.sidecar as sidecar

FIXED_ID = bus.format_msg_id(1)


def _setup_team(monkeypatch, workspace, sent=None):
    """Common test setup: fake team with one agent."""

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"
        cli = "claude"
        model = ""
        session_id = None
        spawned_at = 0.0

        def is_alive(self):
            return True

        def send(self, text):
            if sent is not None:
                sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.agents = {"gpt": _FakeAgent(), "claude": _FakeAgent()}

        def get(self, name):
            if name in ("gpt", "claude"):
                a = _FakeAgent()
                a.name = name
                return a
            raise KeyError(name)

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _t, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _f=None: "claude")
    return _FakeTeam()


def _patch_sidecar_status_requests(monkeypatch):
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    def _request_delivery(workspace: str, message_id: str):
        from hive.sidecar import _delivery_payload

        return _delivery_payload(workspace, {}, message_id)

    monkeypatch.setattr("hive.sidecar.request_delivery", _request_delivery)


# --- delivery ---


def test_delivery_reports_tracking_lost_without_writing_observation(configure_hive_home, tmp_path):
    """High: no obs + not in pending should return failed+reason=tracking_lost and NOT pollute durable store."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="stranded msg",
        message_id="t1",
    )

    payload = sidecar._delivery_payload(str(workspace), {}, "t1")

    assert payload["delivery"] == "failed"
    assert payload["reason"] == "tracking_lost"
    assert payload["recommendedAction"] == "retry"

    observations = [e for e in bus.read_all_events(workspace) if e.get("intent") == "observation"]
    assert observations == []


def test_delivery_reads_legacy_tracking_lost_as_failed(configure_hive_home, tmp_path):
    """Medium: legacy observations with result=tracking_lost must project as failed (not pending)."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="legacy msg",
        message_id="l1",
    )
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="l1",
        metadata={
            "msgId": "l1",
            "result": "tracking_lost",
            "injectStatus": "submitted",
            "turnObserved": "pending",
        },
    )

    payload = sidecar._delivery_payload(str(workspace), {}, "l1")
    assert payload["delivery"] == "failed"


def test_delivery_reports_pending_when_tracker_still_running(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="in-flight msg",
        message_id="q1",
    )

    monkeypatch.setattr(
        "hive.sidecar.request_delivery",
        lambda ws, message_id: sidecar._delivery_payload(ws, {"q1": {}}, message_id),
    )

    result = runner.invoke(cli, ["delivery", "q1"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["msgId"] == "q1"
    assert payload["delivery"] == "pending"
    assert payload["injectStatus"] == "submitted"
    assert payload["turnObserved"] == "pending"


def test_delivery_prefers_observation_result(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="done msg",
        message_id="c1",
    )

    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="c1",
        metadata={
            "msgId": "c1",
            "result": "success",
            "observedAt": "2026-04-14T00:00:00Z",
            "injectStatus": "submitted",
            "turnObserved": "confirmed",
            "confirmationSource": "stream",
        },
    )

    result = runner.invoke(cli, ["delivery", "c1"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["msgId"] == "c1"
    assert payload["delivery"] == "success"
    assert payload["injectStatus"] == "submitted"
    assert payload["turnObserved"] == "confirmed"
    assert payload["confirmationSource"] == "stream"
    assert payload["observedAt"] == "2026-04-14T00:00:00Z"



def test_delivery_failed_reports_retry_guidance(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="broken msg",
        message_id="f1",
    )
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="f1",
        metadata={
            "msgId": "f1",
            "result": "failed",
            "observedAt": "2026-04-14T00:00:00Z",
            "injectStatus": "failed",
            "turnObserved": "unavailable",
        },
    )

    result = runner.invoke(cli, ["delivery", "f1"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "failed"
    assert payload["recommendedAction"] == "retry"
    assert "Delivery failed" in payload["meaning"]


def test_delivery_unconfirmed_reports_cautious_retry_guidance(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="slow msg",
        message_id="u1",
    )
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="u1",
        metadata={
            "msgId": "u1",
            "result": "failed",
            "observedAt": "2026-04-14T00:00:00Z",
            "injectStatus": "submitted",
            "turnObserved": "unconfirmed",
        },
    )

    result = runner.invoke(cli, ["delivery", "u1"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "failed"
    assert payload["recommendedAction"] == "retry"
    assert "Delivery failed" in payload["meaning"]


def test_delivery_pending_record_retains_unconfirmed_state_during_followup(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="slow msg",
        message_id="u2",
    )

    pending = {
        "u2": {
            "terminalNotifiedResult": "failed",
            "terminalFollowupUntil": 9999999999.0,
        }
    }
    monkeypatch.setattr(
        "hive.sidecar.request_delivery",
        lambda ws, message_id: sidecar._delivery_payload(
            ws,
            pending,
            message_id,
        ),
    )

    result = runner.invoke(cli, ["delivery", "u2"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["delivery"] == "failed"
    assert payload["recommendedAction"] == "retry"


# --- doctor ---


def test_doctor_self(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr(
        "hive.sidecar.request_doctor",
        lambda _ws, *, team, target_agent, verbose=False: {
            "ok": True,
            "agent": target_agent,
            "team": team,
            "alive": True,
            "busy": False,
            "model": "gpt-5.4",
            "inputState": "ready",
            "turnPhase": "assistant_text_idle",
            "gate": "clear",
            "transcript": "/tmp/session.jsonl",
            "transcriptSize": 1234,
            "gateReason": "",
        },
    )
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["agent"] == "claude"
    assert payload["team"] == "team-x"
    assert payload["alive"] is True
    assert payload["busy"] is False
    assert payload["model"] == "gpt-5.4"
    assert payload["inputState"] == "ready"
    assert payload["turnPhase"] == "assistant_text_idle"
    assert payload["gate"] == "clear"
    assert payload["transcript"] == "/tmp/session.jsonl"
    assert payload["transcriptSize"] == 1234


def test_doctor_named_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr(
        "hive.sidecar.request_doctor",
        lambda _ws, *, team, target_agent, verbose=False: {
            "ok": True,
            "agent": target_agent,
            "team": team,
            "alive": True,
            "busy": True,
        },
    )
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor", "gpt"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["agent"] == "gpt"
    assert payload["alive"] is True
    assert payload["busy"] is True


def test_doctor_requests_verbose_detail_by_default(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    captured: dict[str, object] = {}

    def _request_doctor(_ws, *, team, target_agent, verbose=False):
        captured["verbose"] = verbose
        return {
            "ok": True,
            "agent": target_agent,
            "team": team,
            "alive": True,
            "busy": False,
            "model": "gpt-5.4",
            "inputState": "ready",
            "gate": "clear",
            "transcript": "/tmp/session.jsonl",
            "transcriptSize": 1234,
            "gateReason": "",
        }

    monkeypatch.setattr("hive.sidecar.request_doctor", _request_doctor)
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert captured["verbose"] is True
    assert payload["transcript"] == "/tmp/session.jsonl"
    assert payload["transcriptSize"] == 1234


def test_doctor_includes_sidecar_metadata(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr(
        "hive.sidecar.request_doctor",
        lambda _ws, *, team, target_agent, verbose=False: {
            "ok": True,
            "agent": target_agent,
            "team": team,
            "alive": True,
            "busy": False,
            "sidecar": {
                "pid": 4242,
                "started_at": "2026-04-17T00:00:00Z",
                "code_hash": "deadbeef",
            },
        },
    )
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["sidecar"] == {
        "pid": 4242,
        "started_at": "2026-04-17T00:00:00Z",
        "code_hash": "deadbeef",
    }


def test_doctor_with_skills_includes_local_skill_diagnostics(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr(
        "hive.sidecar.request_doctor",
        lambda _ws, *, team, target_agent, verbose=False: {
            "ok": True,
            "agent": target_agent,
            "team": team,
            "alive": True,
            "busy": False,
            "model": "gpt-5.4",
            "inputState": "ready",
            "gate": "clear",
        },
    )
    monkeypatch.setattr(
        "hive.cli.skill_sync.diagnose_hive_skill",
        lambda cli: {
            "skill": "hive",
            "cli": cli,
            "state": "stale",
            "recommendedAction": "refresh",
        },
    )
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor", "--skills"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["skills"] == {
        "skill": "hive",
        "cli": "claude",
        "state": "stale",
        "recommendedAction": "refresh",
    }


def test_doctor_unknown_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr(
        "hive.sidecar.request_doctor",
        lambda _ws, *, team, target_agent, verbose=False: {
            "ok": False,
            "error": f"agent '{target_agent}' not registered",
        },
    )
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor", "nobody"])
    assert result.exit_code != 0
    assert "not registered" in result.output


def test_thread_command_outputs_thread_projection(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)
    monkeypatch.setattr(
        "hive.sidecar.request_thread",
        lambda _ws, message_id: {
            "ok": True,
            "rootMsgId": "a001",
            "focusMsgId": message_id,
            "messages": [
                {"msgId": "a001", "from": "momo", "to": "orch", "depth": 0},
                {"msgId": "a002", "from": "orch", "to": "momo", "inReplyTo": "a001", "depth": 1, "focus": True},
            ],
        },
    )

    result = runner.invoke(cli, ["thread", "a002"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["rootMsgId"] == "a001"
    assert payload["focusMsgId"] == "a002"
    assert payload["messages"][1]["focus"] is True

