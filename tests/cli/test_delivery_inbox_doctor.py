"""Tests for hive inbox and doctor commands."""

import json

from hive import bus
from hive.cli import cli

FIXED_ID = "ab12"


def _setup_team(monkeypatch, workspace, sent=None):
    """Common test setup: fake team with one agent."""

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"
        cli = "claude"
        model = ""
        color = "green"
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
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    return _FakeTeam()


# --- delivery ---


def test_delivery_reports_primary_state_and_raw_details(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr("hive.sidecar.check_stale_sidecar", lambda *_args, **_kw: None)

    seq = bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="queued msg",
        message_id="q1",
    )
    bus.patch_event(
        workspace,
        seq,
        injectStatus="submitted",
        turnObserved="pending",
        runtimeQueueState="queued",
        queueSource="capture",
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

    seq = bus.write_event(
        workspace,
        from_agent="claude",
        to_agent="gpt",
        intent="send",
        body="done msg",
        message_id="c1",
    )
    bus.patch_event(
        workspace,
        seq,
        injectStatus="submitted",
        turnObserved="pending",
        runtimeQueueState="queued",
    )

    bus.write_event(
        workspace,
        from_agent="_system",
        to_agent="",
        intent="observation",
        message_id="c1",
        metadata={"msgId": "c1", "result": "confirmed", "observedAt": "2026-04-14T00:00:00Z"},
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


# --- inbox ---


def test_inbox_shows_messages_to_self(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    bus.write_event(
        workspace, from_agent="gpt", to_agent="claude",
        intent="send", body="hello claude", message_id="m1",
    )

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 1
    assert payload["messages"][0]["body"] == "hello claude"


def test_inbox_advances_cursor(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    bus.write_event(
        workspace, from_agent="gpt", to_agent="claude",
        intent="send", body="first", message_id="m1",
    )

    runner.invoke(cli, ["inbox"])

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 0


def test_inbox_does_not_misreport_tracking_lost(runner, configure_hive_home, monkeypatch, tmp_path):
    """Messages sent with --wait or unavailable should NOT trigger tracking_lost."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    seq = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="waited msg", message_id="w1",
    )
    bus.patch_event(
        workspace,
        seq,
        injectStatus="submitted",
        turnObserved="confirmed",
    )

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0

    from hive.observer import find_observation
    obs = find_observation(str(workspace), "w1")
    assert obs is None


def test_inbox_tracking_lost_not_repeated(runner, configure_hive_home, monkeypatch, tmp_path):
    """tracking_lost should only appear once, not on every subsequent inbox call."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    seq = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="pending msg", message_id="p1",
    )
    bus.patch_event(
        workspace,
        seq,
        injectStatus="submitted",
        turnObserved="pending",
    )

    result1 = runner.invoke(cli, ["inbox"])
    assert result1.exit_code == 0
    p1 = json.loads(result1.output)
    assert p1["unread"] >= 1

    result2 = runner.invoke(cli, ["inbox"])
    assert result2.exit_code == 0
    p2 = json.loads(result2.output)
    assert p2["unread"] == 0


# --- doctor ---


def test_doctor_self(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: None)

    result = runner.invoke(cli, ["doctor"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["agent"] == "claude"
    assert payload["team"] == "team-x"
    assert payload["alive"] is True


def test_doctor_named_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: None)

    result = runner.invoke(cli, ["doctor", "gpt"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["agent"] == "gpt"
    assert payload["alive"] is True


def test_doctor_unknown_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    result = runner.invoke(cli, ["doctor", "nobody"])
    assert result.exit_code != 0
    assert "not registered" in result.output
