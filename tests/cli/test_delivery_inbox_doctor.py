"""Tests for hive delivery, inbox, and doctor commands."""

import json
import os

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


def test_delivery_not_found(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    result = runner.invoke(cli, ["delivery", "xxxx"])
    assert result.exit_code != 0
    assert "no send event found" in result.output


def test_delivery_shows_persisted_status_no_observer(runner, configure_hive_home, monkeypatch, tmp_path):
    """Messages sent with --wait have no observerPid; delivery shows persisted turnObserved."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    path = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="hello", message_id=FIXED_ID,
    )
    data = json.loads(path.read_text())
    data["injectStatus"] = "submitted"
    data["turnObserved"] = "confirmed"
    path.write_text(json.dumps(data) + "\n")

    result = runner.invoke(cli, ["delivery", FIXED_ID])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["injectStatus"] == "submitted"
    assert payload["turnObserved"] == "confirmed"
    assert "followUp" not in payload


def test_delivery_detects_stale_observer(runner, configure_hive_home, monkeypatch, tmp_path):
    """When observer PID is gone and no observation, delivery writes observer_lost."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    path = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="hello", message_id=FIXED_ID,
    )
    data = json.loads(path.read_text())
    data["injectStatus"] = "submitted"
    data["turnObserved"] = "pending"
    data["observerPid"] = 99999  # non-existent PID
    path.write_text(json.dumps(data) + "\n")

    result = runner.invoke(cli, ["delivery", FIXED_ID])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "observer_lost"
    assert payload["followUp"]["command"] == "hive doctor gpt"


def test_delivery_finds_observation_event(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="hello", message_id=FIXED_ID,
    )
    # Write an observation event
    from hive.observer import _write_observation
    _write_observation(str(workspace), FIXED_ID, "confirmed")

    result = runner.invoke(cli, ["delivery", FIXED_ID])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "confirmed"
    assert "followUp" not in payload


def test_delivery_latest_observation_wins(runner, configure_hive_home, monkeypatch, tmp_path):
    """If multiple observations exist, the latest one wins."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="hello", message_id=FIXED_ID,
    )
    from hive.observer import _write_observation
    _write_observation(str(workspace), FIXED_ID, "observer_lost")
    import time; time.sleep(0.001)  # ensure different ns timestamp
    _write_observation(str(workspace), FIXED_ID, "confirmed")

    result = runner.invoke(cli, ["delivery", FIXED_ID])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "confirmed"  # latest wins, not earliest
    assert "followUp" not in payload


def test_delivery_pending_includes_follow_up(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    path = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="hello", message_id=FIXED_ID,
    )
    data = json.loads(path.read_text())
    data["injectStatus"] = "submitted"
    data["turnObserved"] = "pending"
    data["observerPid"] = os.getpid()
    path.write_text(json.dumps(data) + "\n")

    result = runner.invoke(cli, ["delivery", FIXED_ID])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "pending"
    assert payload["followUp"]["command"] == f"hive delivery {FIXED_ID}"
    assert payload["followUp"]["suggestedAfterSec"] == 10


def test_delivery_unconfirmed_includes_diagnose_then_resend_guidance(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    path = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="hello", message_id=FIXED_ID,
    )
    data = json.loads(path.read_text())
    data["injectStatus"] = "submitted"
    data["turnObserved"] = "unconfirmed"
    path.write_text(json.dumps(data) + "\n")

    result = runner.invoke(cli, ["delivery", FIXED_ID])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "unconfirmed"
    assert payload["followUp"]["command"] == "hive doctor gpt"
    assert "consider resending" in payload["followUp"]["afterDiagnosis"]


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

    # Second inbox should show 0 unread
    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["unread"] == 0


def test_inbox_does_not_misreport_observer_lost(runner, configure_hive_home, monkeypatch, tmp_path):
    """Messages sent with --wait or unavailable should NOT trigger observer_lost."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    # Simulate a send with --wait (no observerPid)
    path = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="waited msg", message_id="w1",
    )
    data = json.loads(path.read_text())
    data["injectStatus"] = "submitted"
    data["turnObserved"] = "confirmed"
    path.write_text(json.dumps(data) + "\n")

    result = runner.invoke(cli, ["inbox"])
    assert result.exit_code == 0

    # No observer_lost events should have been created
    from hive.observer import find_observation
    obs = find_observation(str(workspace), "w1")
    assert obs is None  # No spurious observation written


# --- doctor ---


def test_inbox_observer_lost_not_repeated(runner, configure_hive_home, monkeypatch, tmp_path):
    """observer_lost should only appear once, not on every subsequent inbox call."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)

    # Simulate a send with a dead observer
    path = bus.write_event(
        workspace, from_agent="claude", to_agent="gpt",
        intent="send", body="pending msg", message_id="p1",
    )
    data = json.loads(path.read_text())
    data["injectStatus"] = "submitted"
    data["turnObserved"] = "pending"
    data["observerPid"] = 99999  # non-existent
    path.write_text(json.dumps(data) + "\n")

    # First inbox: should detect stale observer, report observer_lost
    result1 = runner.invoke(cli, ["inbox"])
    assert result1.exit_code == 0
    p1 = json.loads(result1.output)
    assert p1["unread"] >= 1

    # Second inbox: cursor should have advanced past the observer_lost event
    result2 = runner.invoke(cli, ["inbox"])
    assert result2.exit_code == 0
    p2 = json.loads(result2.output)
    assert p2["unread"] == 0  # No repeats


def test_doctor_self(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    fake_team = _setup_team(monkeypatch, workspace)

    # doctor needs detect_profile_for_pane — mock it
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
