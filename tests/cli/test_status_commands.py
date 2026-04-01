import json

from hive.cli import cli


def test_status_exposes_lead_session_id(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": "orch-session-456")
    monkeypatch.setattr("hive.team.resolve_session_id_for_pane", lambda _pane: "orch-session-456")
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0
    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["self"] == "orch"
    assert payload["tmuxWindow"] == "dev:0"
    orch = next(member for member in payload["members"] if member["name"] == "orch")
    assert orch["role"] == "agent"
    assert orch["sessionId"] == "orch-session-456"

    orch_snapshot = json.loads((workspace / "presence" / "orch.json").read_text())
    assert orch_snapshot["sessionId"] == "orch-session-456"


def test_status_set_show_and_wait_status(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0

    class _FakeTeam:
        def __init__(self):
            self.name = "team-status"
            self.workspace = str(workspace)
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"

        def status(self) -> dict:
            return {
                "members": [
                    {"name": "orch"},
                    {"name": "claude"},
                ]
            }

    monkeypatch.setattr("hive.cli._load_team", lambda _team: _FakeTeam())

    result = runner.invoke(
        cli,
        [
            "status-set",
            "done",
            "review complete",
            "--agent",
            "claude",
            "--meta",
            "cr.review=done",
            "--meta",
            "artifact=/tmp/review.md",
        ],
    )
    assert result.exit_code == 0
    set_payload = json.loads(result.output)
    assert set_payload["agent"] == "claude"
    assert set_payload["state"] == "done"
    assert set_payload["summary"] == "review complete"
    assert set_payload["path"].endswith("/status/claude.json")

    show_result = runner.invoke(cli, ["status", "--agent", "claude"])
    assert show_result.exit_code == 0
    payload = json.loads(show_result.output)
    assert payload["state"] == "done"
    assert payload["summary"] == "review complete"
    assert payload["metadata"] == {"cr.review": "done", "artifact": "/tmp/review.md"}

    wait_result = runner.invoke(
        cli,
        [
            "wait-status",
            "claude",
            "--state",
            "done",
            "--meta",
            "cr.review=done",
            "--timeout",
            "1",
        ],
    )
    assert wait_result.exit_code == 0
    payload = json.loads("\n".join(wait_result.output.splitlines()[1:]))
    assert payload["metadata"]["artifact"] == "/tmp/review.md"


def test_status_without_agent_returns_all_statuses(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0

    class _FakeTeam:
        def __init__(self):
            self.name = "team-status"
            self.workspace = str(workspace)
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"

        def status(self) -> dict:
            return {
                "members": [
                    {"name": "orch"},
                    {"name": "claude"},
                    {"name": "gpt"},
                ]
            }

    monkeypatch.setattr("hive.cli._load_team", lambda _team: _FakeTeam())

    assert runner.invoke(
        cli,
        ["status-set", "busy", "working", "--agent", "claude"],
    ).exit_code == 0
    assert runner.invoke(
        cli,
        ["status-set", "done", "finished", "--agent", "gpt"],
    ).exit_code == 0

    result = runner.invoke(cli, ["status"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["claude"]["state"] == "busy"
    assert payload["gpt"]["state"] == "done"


def test_statuses_filters_out_stale_agents_from_team_view(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-w", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["status-set", "busy", "working", "--agent", "orch"]).exit_code == 0
    assert runner.invoke(cli, ["status-set", "busy", "ghost", "--agent", "ghost"]).exit_code == 0

    result = runner.invoke(cli, ["status"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert "orch" in payload
    assert "ghost" not in payload


def test_wait_status_times_out_without_match(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-timeout", "--workspace", str(workspace)]).exit_code == 0

    result = runner.invoke(
        cli,
        ["wait-status", "claude", "--state", "done", "--timeout", "0"],
    )

    assert result.exit_code != 0
    assert "Timed out" in result.output


def test_wait_status_fails_when_agent_dies(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    for name in ("artifacts", "status", "presence", "state"):
        (workspace / name).mkdir(parents=True, exist_ok=True)

    class _FakeAgent:
        def capture(self, _lines: int) -> str:
            return "agent tail"

    class _FakeTeam:
        def __init__(self):
            self.name = "team-x"
            self.workspace = str(workspace)
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"
            self.lead_name = "orch"

        def status(self) -> dict:
            return {"members": [{"name": "claude", "alive": False}]}

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(
        cli,
        ["wait-status", "claude", "--state", "done"],
    )

    assert result.exit_code != 0
    assert "no longer alive" in result.output
    assert "agent tail" in result.output


def test_who_includes_terminals(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-w", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["terminal", "add", "term-1", "--pane", "%77"]).exit_code == 0

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    terminal = next(member for member in payload["members"] if member["name"] == "term-1")
    assert terminal["role"] == "terminal"
