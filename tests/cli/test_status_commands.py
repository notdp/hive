import json

from hive.cli import cli


def test_status_exposes_lead_session_id(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.team.detect_current_session_id", lambda _cwd: "orch-session-456")
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0
    result = runner.invoke(cli, ["status", "-t", "team-status"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["agents"]["orchestrator"]["sessionId"] == "orch-session-456"

    orchestrator_snapshot = json.loads((workspace / "presence" / "orchestrator.json").read_text())
    assert orchestrator_snapshot["sessionId"] == "orch-session-456"


def test_status_set_show_and_wait_status(runner, tmp_path):
    workspace = tmp_path / "ws"
    for name in ("artifacts", "status", "presence", "state"):
        (workspace / name).mkdir(parents=True, exist_ok=True)

    result = runner.invoke(
        cli,
        [
            "status-set",
            "done",
            "review complete",
            "--workspace",
            str(workspace),
            "--agent",
            "claude",
            "--meta",
            "cr.review=done",
            "--meta",
            "artifact=/tmp/review.md",
        ],
    )
    assert result.exit_code == 0

    show_result = runner.invoke(cli, ["status-show", "--workspace", str(workspace), "--agent", "claude"])
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
            "--workspace",
            str(workspace),
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


def test_status_show_without_agent_returns_all_statuses(runner, tmp_path):
    workspace = tmp_path / "ws"
    for name in ("artifacts", "status", "presence", "state"):
        (workspace / name).mkdir(parents=True, exist_ok=True)

    assert runner.invoke(
        cli,
        ["status-set", "busy", "working", "--workspace", str(workspace), "--agent", "claude"],
    ).exit_code == 0
    assert runner.invoke(
        cli,
        ["status-set", "done", "finished", "--workspace", str(workspace), "--agent", "gpt"],
    ).exit_code == 0

    result = runner.invoke(cli, ["status-show", "--workspace", str(workspace)])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["claude"]["state"] == "busy"
    assert payload["gpt"]["state"] == "done"


def test_wait_status_times_out_without_match(runner, tmp_path):
    workspace = tmp_path / "ws"
    for name in ("artifacts", "status", "presence", "state"):
        (workspace / name).mkdir(parents=True, exist_ok=True)

    result = runner.invoke(
        cli,
        ["wait-status", "claude", "--workspace", str(workspace), "--state", "done", "--timeout", "0"],
    )

    assert result.exit_code != 0
    assert "Timed out" in result.output


def test_wait_status_fails_when_agent_dies(runner, monkeypatch, tmp_path):
    workspace = tmp_path / "ws"
    for name in ("artifacts", "status", "presence", "state"):
        (workspace / name).mkdir(parents=True, exist_ok=True)

    class _FakeAgent:
        def capture(self, _lines: int) -> str:
            return "agent tail"

    class _FakeTeam:
        def status(self) -> dict:
            return {"agents": {"claude": {"alive": False}}}

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._load_team", lambda _team: _FakeTeam())

    result = runner.invoke(
        cli,
        ["wait-status", "claude", "--team", "team-x", "--workspace", str(workspace), "--state", "done"],
    )

    assert result.exit_code != 0
    assert "no longer alive" in result.output
    assert "agent tail" in result.output


def test_who_includes_terminals(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-w", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["terminal", "add", "term-1", "-t", "team-w", "--pane", "%77"]).exit_code == 0

    result = runner.invoke(cli, ["who", "-t", "team-w"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert "term-1" in payload["terminals"]
