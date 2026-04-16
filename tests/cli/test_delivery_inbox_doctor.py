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


def test_delivery_reports_primary_state_and_raw_details(runner, configure_hive_home, monkeypatch, tmp_path):
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
        body="queued msg",
        message_id="q1",
    )

    monkeypatch.setattr(
        "hive.sidecar.request_delivery",
        lambda ws, message_id: sidecar._delivery_payload(
            ws,
            {
                "q1": {
                    "runtimeQueueState": "queued",
                    "queueSource": "capture",
                }
            },
            message_id,
        ),
    )

    result = runner.invoke(cli, ["delivery", "q1"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["msgId"] == "q1"
    assert payload["state"] == "queued"
    assert payload["injectStatus"] == "submitted"
    assert payload["turnObserved"] == "pending"
    assert payload["runtimeQueueState"] == "queued"
    assert payload["queueSource"] == "capture"


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
            "result": "confirmed",
            "observedAt": "2026-04-14T00:00:00Z",
            "injectStatus": "submitted",
            "turnObserved": "confirmed",
            "runtimeQueueState": "queued",
        },
    )

    result = runner.invoke(cli, ["delivery", "c1"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["msgId"] == "c1"
    assert payload["state"] == "confirmed"
    assert payload["injectStatus"] == "submitted"
    assert payload["turnObserved"] == "confirmed"
    assert payload["runtimeQueueState"] == "queued"
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
    assert payload["state"] == "failed"
    assert payload["recommendedAction"] == "retry"
    assert "failed before delivery tracking began" in payload["meaning"]


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
            "result": "unconfirmed",
            "observedAt": "2026-04-14T00:00:00Z",
            "injectStatus": "submitted",
            "turnObserved": "unconfirmed",
        },
    )

    result = runner.invoke(cli, ["delivery", "u1"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["state"] == "unconfirmed"
    assert payload["recommendedAction"] == "cautious_retry"
    assert "not confirmed" in payload["meaning"]


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
            "model": "gpt-5.4",
            "inputState": "ready",
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
    assert payload["model"] == "gpt-5.4"
    assert payload["inputState"] == "ready"
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
        },
    )
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor", "gpt"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["agent"] == "gpt"
    assert payload["alive"] is True


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


# --- hidden agent-facing views ---


def test_suggest_command_outputs_candidates(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)
    monkeypatch.setattr(
        "hive.sidecar.request_suggest",
        lambda _ws, *, team, source_agent: {
            "ok": True,
            "team": team,
            "source": {"name": source_agent, "cli": "codex", "model": "gpt-5.4"},
            "candidates": [
                {
                    "name": "orch",
                    "role": "agent",
                    "pane": "%2",
                    "cli": "claude",
                    "model": "claude-opus-4-6",
                    "alive": True,
                    "inputState": "ready",
                    "score": 140,
                    "reasons": ["ready", "different_model", "different_cli"],
                }
            ],
        },
    )

    result = runner.invoke(cli, ["suggest"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["source"]["name"] == "claude"
    assert payload["candidates"][0]["name"] == "orch"
    assert payload["candidates"][0]["score"] == 140


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


def test_activity_command_outputs_activity_probe(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)
    monkeypatch.setattr(
        "hive.sidecar.request_doctor",
        lambda _ws, *, team, target_agent, verbose=False: {
            "ok": True,
            "agent": target_agent,
            "team": team,
            "alive": True,
            "inputState": "ready",
            "cli": "claude",
            "pane": "%99",
            "transcript": "/tmp/session.jsonl",
            "transcriptExists": True,
            "transcriptSize": 1234,
            "activityState": "active",
            "activityReason": "assistant_tool_use_open",
            "activityObservedAt": "2026-04-16T05:00:00Z",
            "activityRole": "assistant",
            "activityPartKinds": ["tool_use"],
            "activityEvidence": {
                "tail": [
                    {
                        "role": "assistant",
                        "partKinds": ["tool_use"],
                        "observedAt": "2026-04-16T05:00:00Z",
                    }
                ]
            },
        },
    )

    result = runner.invoke(cli, ["activity"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["agent"] == "claude"
    assert payload["activityState"] == "active"
    assert payload["activityReason"] == "assistant_tool_use_open"
    assert payload["activityRole"] == "assistant"
    assert payload["activityPartKinds"] == ["tool_use"]
