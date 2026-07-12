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
            "transcript": "/tmp/session.jsonl",
            "transcriptSize": 1234,
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
    assert "gate" not in payload
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


def test_doctor_reports_duplicate_team_bindings_without_repair(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """`hive doctor` surfaces windows colliding on a team name (Bug A) — reporting
    both windows/ids/workspaces/members — and never auto-repairs."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _setup_team(monkeypatch, workspace)
    monkeypatch.setattr(
        "hive.sidecar.request_doctor",
        lambda _ws, *, team, target_agent, verbose=False: {
            "ok": True, "agent": target_agent, "team": team, "alive": True,
        },
    )
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    dupe = {
        "team": "0-2",
        "windows": [
            {
                "tmuxWindow": "0:2", "windowId": "@2", "workspace": "/tmp/hive-0-w2",
                "liveMembers": [
                    {"name": "worker", "pane": "%42", "group": "duo"},
                    {"name": "validator", "pane": "%45", "group": "duo"},
                ],
            },
            {
                "tmuxWindow": "0:3", "windowId": "@3", "workspace": "/tmp/hive-0-w3",
                "liveMembers": [
                    {"name": "worker", "pane": "%10", "group": "duo"},
                    {"name": "validator", "pane": "%40", "group": "duo"},
                ],
            },
        ],
        "repair": "manual: two windows claim this team; do not auto-retag a live team",
    }
    cleared: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.team.duplicate_team_bindings", lambda: [dupe])
    monkeypatch.setattr("hive.cli.tmux.clear_window_option", lambda wt, key: cleared.append((wt, key)))

    result = runner.invoke(cli, ["doctor"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    dups = payload["duplicateTeams"]
    assert dups[0]["team"] == "0-2"
    assert {w["windowId"] for w in dups[0]["windows"]} == {"@2", "@3"}
    assert dups[0]["windows"][0]["liveMembers"][0]["name"] == "worker"
    assert "manual" in dups[0]["repair"]
    assert cleared == []  # detection only — no auto-repair


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
            "transcript": "/tmp/session.jsonl",
            "transcriptSize": 1234,
        }

    monkeypatch.setattr("hive.sidecar.request_doctor", _request_doctor)
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    result = runner.invoke(cli, ["doctor"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert captured["verbose"] is True
    assert payload["transcript"] == "/tmp/session.jsonl"
    assert payload["transcriptSize"] == 1234


def test_doctor_payload_includes_log_paths(configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self):
            return True

    class _FakeTeam:
        name = "team-x"
        agents = {"gpt": _FakeAgent()}

        def get(self, name):
            if name == "gpt":
                return _FakeAgent()
            raise KeyError(name)

    monkeypatch.setattr("hive.team.Team.load", lambda _team_name: _FakeTeam())
    monkeypatch.setattr(
        sidecar,
        "_member_runtime_payload",
        lambda _pane_id, role="agent": {"alive": True, "inputState": "ready"},
    )

    payload = sidecar._doctor_payload(str(workspace), "team-x", "gpt", verbose=True)

    assert payload["runDir"] == str(workspace / "run")
    assert payload["logs"] == {
        "notify": str(workspace / "run" / "notify.jsonl"),
        "sidecar_stderr": str(workspace / "run" / "sidecar.stderr"),
        "cvim_dir": str(workspace / "run" / "cvim"),
    }


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


def test_doctor_rejects_removed_skills_option(runner, configure_hive_home):
    configure_hive_home()
    result = runner.invoke(cli, ["doctor", "--skills"])
    assert result.exit_code != 0
    assert "--skills" in result.output  # click names the unknown option


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


def test_delivery_command_is_removed(runner):
    """Delivery is binary at send time; there is no state left to query."""
    result = runner.invoke(cli, ["delivery", "q1"])
    assert result.exit_code == 2
    assert "No such command 'delivery'" in result.output
