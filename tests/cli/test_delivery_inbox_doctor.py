"""Tests for hive inbox and doctor commands."""

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

    def _request_inbox(workspace: str, *, agent_name: str, ack: bool = False):
        from hive.sidecar import _inbox_payload

        return _inbox_payload(workspace, {}, agent_name, ack)

    monkeypatch.setattr("hive.sidecar.request_delivery", _request_delivery)
    monkeypatch.setattr("hive.sidecar.request_inbox", _request_inbox)


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


# --- inbox ---


def test_inbox_shows_messages_to_self(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace, from_agent="gpt", to_agent="claude",
        intent="send", body="hello claude", message_id="m1",
    )

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 1
    assert payload["messages"][0]["body"] == "hello claude"


def test_inbox_hides_sender_transport_fields_for_incoming_messages(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace,
        from_agent="gpt",
        to_agent="claude",
        intent="send",
        body="transport details should stay hidden",
        message_id="m2",
    )

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    message = payload["messages"][0]
    assert message["msgId"] == "m2"
    assert message["body"] == "transport details should stay hidden"
    assert "injectStatus" not in message
    assert "turnObserved" not in message
    assert "runtimeQueueState" not in message
    assert "queueSource" not in message


def test_inbox_does_not_advance_cursor_by_default(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace, from_agent="gpt", to_agent="claude",
        intent="send", body="first", message_id="m1",
    )

    runner.invoke(cli, ["inbox"])

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 1


def test_inbox_ack_advances_cursor(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace, from_agent="gpt", to_agent="claude",
        intent="send", body="first", message_id="m1",
    )

    runner.invoke(cli, ["inbox", "--ack"])

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 0


def test_inbox_ignores_self_observations(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="waited msg", message_id="w1",
    )
    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="w1",
        metadata={
            "msgId": "w1",
            "result": "confirmed",
            "observedAt": "2026-04-14T00:00:00Z",
            "injectStatus": "submitted",
            "turnObserved": "confirmed",
        },
    )

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 0
    assert payload["messages"] == []


def test_inbox_does_not_synthesize_tracking_lost_for_self_sends(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    _patch_sidecar_status_requests(monkeypatch)

    bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="pending msg", message_id="p1",
    )

    result = runner.invoke(cli, ["inbox", "--ack"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 0
    assert payload["messages"] == []

    from hive.observer import find_observation

    obs = find_observation(str(workspace), "p1")
    assert obs is None


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
