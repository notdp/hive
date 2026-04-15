import json
import os
from types import SimpleNamespace

from hive.adapters.base import GateResult
from hive import bus
from hive.cli import cli
import hive.sidecar as sidecar
from hive import tmux


def _patch_runtime(monkeypatch, runtime_payload):
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)
    monkeypatch.setattr(
        "hive.sidecar.request_team_runtime",
        lambda _ws, *, team: {"ok": True, "team": team, **runtime_payload},
    )


def test_status_exposes_lead_session_id_via_daemon(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": "orch-session-456")
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0
    _patch_runtime(
        monkeypatch,
        {
            "members": {
                "orch": {
                    "alive": True,
                    "sessionId": "orch-session-456",
                    "model": "gpt-5.4",
                    "inputState": "ready",
                    "inputReason": "",
                }
            }
        },
    )
    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["self"] == "orch"
    assert payload["tmuxWindow"] == "dev:0"
    orch = next(member for member in payload["members"] if member["name"] == "orch")
    assert orch["role"] == "agent"
    assert orch["sessionId"] == "orch-session-456"
    assert orch["model"] == "gpt-5.4"
    assert orch["inputState"] == "ready"


def test_current_uses_daemon_for_model(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": "orch-session-456")
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-current", "--workspace", str(workspace)]).exit_code == 0
    _patch_runtime(
        monkeypatch,
        {
            "members": {
                "orch": {
                    "alive": True,
                    "model": "gpt-5.4",
                }
            }
        },
    )

    result = runner.invoke(cli, ["current"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "team-current"
    assert payload["agent"] == "orch"
    assert payload["model"] == "gpt-5.4"


def test_legacy_status_commands_show_migration_error(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0

    status_set_result = runner.invoke(cli, ["status-set", "busy", "working", "--agent", "claude"])
    assert status_set_result.exit_code != 0
    assert "`hive status-set` was removed" in status_set_result.output
    assert "hive send" in status_set_result.output

    wait_result = runner.invoke(cli, ["wait-status", "claude", "--state", "done"])
    assert wait_result.exit_code != 0
    assert "`hive wait-status` was removed" in wait_result.output

    status_result = runner.invoke(cli, ["status", "--agent", "claude"])
    assert status_result.exit_code != 0
    assert "`hive status` was removed" in status_result.output
    assert "hive team" in status_result.output

    statuses_result = runner.invoke(cli, ["statuses"])
    assert statuses_result.exit_code != 0
    assert "`hive statuses` was removed" in statuses_result.output

    status_show_result = runner.invoke(cli, ["status-show"])
    assert status_show_result.exit_code != 0
    assert "`hive status-show` was removed" in status_show_result.output


def test_team_includes_needs_answer_from_daemon(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-na", "--workspace", str(workspace)]).exit_code == 0

    _patch_runtime(
        monkeypatch,
        {
            "members": {
                "orch": {
                    "alive": True,
                    "inputState": "waiting_user",
                    "inputReason": "ask_pending",
                    "pendingQuestion": "proceed?",
                }
            },
            "needsAnswer": ["orch"],
        },
    )

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["needsAnswer"] == ["orch"]
    orch = next(member for member in payload["members"] if member["name"] == "orch")
    assert orch["pendingQuestion"] == "proceed?"


def test_who_includes_terminals(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-w", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["terminal", "add", "term-1", "--pane", "%77"]).exit_code == 0
    _patch_runtime(monkeypatch, {"members": {"orch": {"alive": True}, "term-1": {"alive": False}}})

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    terminal = next(member for member in payload["members"] if member["name"] == "term-1")
    assert terminal["role"] == "terminal"
    assert terminal["alive"] is False


def test_team_runtime_keeps_distinct_claude_sessions_for_same_window(
    runner, configure_hive_home, monkeypatch, tmp_path,
):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    claude_home = tmp_path / "claude-home"
    sessions_dir = claude_home / "sessions"
    projects_dir = claude_home / "projects" / "-repo"
    sessions_dir.mkdir(parents=True)
    projects_dir.mkdir(parents=True)
    (sessions_dir / "42424.json").write_text(json.dumps({"sessionId": "sess-old"}))
    (sessions_dir / "52525.json").write_text(json.dumps({"sessionId": "sess-new"}))

    stale = projects_dir / "sess-old.jsonl"
    stale.write_text(json.dumps({"sessionId": "sess-old", "cwd": "/repo"}) + "\n")
    fresh = projects_dir / "sess-new.jsonl"
    fresh.write_text(json.dumps({"sessionId": "sess-new", "cwd": "/repo"}) + "\n")
    stale_ns = 1_700_000_000_000_000_000
    fresh_ns = stale_ns + 5_000
    os.utime(stale, ns=(stale_ns, stale_ns))
    os.utime(fresh, ns=(fresh_ns, fresh_ns))

    class _FakeAgent:
        def __init__(self, name: str, pane_id: str):
            self.name = name
            self.pane_id = pane_id
            self.cli = "claude"

        def is_alive(self) -> bool:
            return True

    class _FakeTeam:
        def __init__(self):
            self.name = "team-x"
            self.description = "demo"
            self.workspace = str(workspace)
            self.tmux_session = "dev"
            self.tmux_window = "dev:4"
            self.agents = {
                "bobo": _FakeAgent("bobo", "%2000"),
                "orch": _FakeAgent("orch", "%1070"),
            }
            self.terminals = {}

        def lead_agent(self):
            return None

        def status(self):
            return {
                "name": self.name,
                "description": self.description,
                "workspace": self.workspace,
                "tmuxSession": self.tmux_session,
                "tmuxWindow": self.tmux_window,
                "members": [
                    {"name": "bobo", "role": "agent", "pane": "%2000"},
                    {"name": "orch", "role": "agent", "pane": "%1070"},
                ],
            }

    fake_team = _FakeTeam()

    monkeypatch.setenv("CLAUDE_HOME", str(claude_home))
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", fake_team))
    monkeypatch.setattr("hive.cli._discover_tmux_binding", lambda: {"team": "team-x", "agent": "orch"})
    monkeypatch.setattr("hive.team.Team.load", lambda _team_name: fake_team)
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)
    monkeypatch.setattr(
        "hive.sidecar.request_team_runtime",
        lambda _ws, *, team: sidecar._team_runtime_payload(team),
    )
    monkeypatch.setattr("hive.sidecar.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.agent_cli.resolve_model_for_pane", lambda *_a, **_kw: "")
    monkeypatch.setattr("hive.adapters.base.check_input_gate", lambda _path: GateResult("clear", ""))
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda _pane_id: True)
    monkeypatch.setattr("hive.tmux.display_value", lambda _target, _fmt: "/repo")
    monkeypatch.setattr("hive.tmux.get_pane_window_target", lambda _pane_id: "dev:4")
    monkeypatch.setattr(
        "hive.tmux.list_panes_full",
        lambda _target: [
            tmux.PaneInfo("%1070", "orch", command="node"),
            tmux.PaneInfo("%2000", "bobo", command="node"),
        ],
    )
    monkeypatch.setattr(
        "hive.tmux.get_pane_tty",
        lambda pane_id: "/dev/ttys001" if pane_id == "%1070" else "/dev/ttys002",
    )

    def _list_tty_processes(tty: str):
        if tty == "/dev/ttys001":
            return [tmux.TTYProcessInfo(pid="42424", command="claude", argv="claude --verbose")]
        if tty == "/dev/ttys002":
            return [tmux.TTYProcessInfo(pid="52525", command="claude", argv="claude --verbose")]
        return []

    monkeypatch.setattr("hive.tmux.list_tty_processes", _list_tty_processes)

    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    orch = next(member for member in payload["members"] if member["name"] == "orch")
    bobo = next(member for member in payload["members"] if member["name"] == "bobo")
    assert orch["sessionId"] == "sess-old"
    assert bobo["sessionId"] == "sess-new"
