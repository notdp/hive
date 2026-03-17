import json

from hive.cli import cli


def test_create_initializes_workspace_and_state(runner, configure_hive_home, tmp_path):
    hive_home = configure_hive_home()
    workspace = tmp_path / "ws"

    result = runner.invoke(
        cli,
        [
            "create",
            "team-a",
            "--workspace",
            str(workspace),
            "--state",
            "repo=owner/repo",
            "--state",
            "pr-number=123",
        ],
    )

    assert result.exit_code == 0
    assert (workspace / "state" / "repo").read_text() == "owner/repo"
    assert (workspace / "state" / "pr-number").read_text() == "123"
    assert (workspace / "artifacts").is_dir()
    assert (workspace / "status").is_dir()
    assert (workspace / "presence").is_dir()

    config = json.loads((hive_home / "teams" / "team-a" / "config.json").read_text())
    assert config["workspace"] == str(workspace)
    current = json.loads((hive_home / "contexts" / "default.json").read_text())
    assert current == {"team": "team-a", "workspace": str(workspace), "agent": "orchestrator"}


def test_create_persists_lead_session_id(runner, configure_hive_home, monkeypatch, tmp_path):
    hive_home = configure_hive_home()
    monkeypatch.setattr("hive.team.detect_current_session_id", lambda _cwd: "orch-session-123")
    workspace = tmp_path / "ws"

    result = runner.invoke(cli, ["create", "team-session", "--workspace", str(workspace)])

    assert result.exit_code == 0
    config = json.loads((hive_home / "teams" / "team-session" / "config.json").read_text())
    assert config["leadSessionId"] == "orch-session-123"


def test_create_rejects_state_without_workspace(runner, configure_hive_home):
    configure_hive_home()

    result = runner.invoke(cli, ["create", "team-a", "--state", "repo=owner/repo"])

    assert result.exit_code != 0
    assert "--state requires --workspace" in result.output


def test_delete_removes_workspace(runner, configure_hive_home, tmp_path):
    hive_home = configure_hive_home()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-b", "--workspace", str(workspace)]).exit_code == 0
    (workspace / "results").mkdir(parents=True, exist_ok=True)
    (workspace / "results" / "x.txt").write_text("ok")

    result = runner.invoke(cli, ["delete", "team-b"])
    assert result.exit_code == 0
    assert not workspace.exists()
    assert not (hive_home / "contexts" / "default.json").exists()


def test_delete_clears_terminal_tags(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    cleared = []
    monkeypatch.setattr("hive.team.tmux.clear_pane_tags", lambda pane_id: cleared.append(pane_id))
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-d2", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["terminal", "add", "t1", "-t", "team-d2", "--pane", "%88"]).exit_code == 0

    result = runner.invoke(cli, ["delete", "team-d2"])

    assert result.exit_code == 0
    assert "%0" in cleared
    assert "%88" in cleared
