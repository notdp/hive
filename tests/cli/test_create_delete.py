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
    assert (workspace / "run").is_dir()
    assert (workspace / "hive.db").is_file()

    # Team state now lives in tmux window options, not config.json
    from hive.team import Team
    team = Team.load("team-a")
    assert team.workspace == str(workspace)

    current = json.loads((hive_home / "contexts" / "pane-0.json").read_text())
    assert current == {"team": "team-a", "workspace": str(workspace), "agent": "orch"}


def test_create_persists_lead_session_id(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": "orch-session-123")
    workspace = tmp_path / "ws"

    result = runner.invoke(cli, ["create", "team-session", "--workspace", str(workspace)])

    assert result.exit_code == 0
    # Lead session ID is resolved at runtime via tmux pane, no longer persisted to config.json
    from hive.team import Team
    team = Team.load("team-session")
    assert team.name == "team-session"


def test_create_rejects_state_without_workspace(runner, configure_hive_home):
    configure_hive_home()

    result = runner.invoke(cli, ["create", "team-a", "--state", "repo=owner/repo"])

    assert result.exit_code != 0
    assert "--state requires --workspace" in result.output


def test_delete_preserves_workspace_by_default(runner, configure_hive_home, tmp_path):
    hive_home = configure_hive_home()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-b", "--workspace", str(workspace)]).exit_code == 0
    (workspace / "results").mkdir(parents=True, exist_ok=True)
    (workspace / "results" / "x.txt").write_text("ok")

    result = runner.invoke(cli, ["delete", "team-b"])
    assert result.exit_code == 0
    assert workspace.exists()  # workspace preserved by default
    assert not (hive_home / "contexts" / "pane-0.json").exists()


def test_delete_removes_workspace_with_flag(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-b2", "--workspace", str(workspace)]).exit_code == 0
    (workspace / "results").mkdir(parents=True, exist_ok=True)

    result = runner.invoke(cli, ["delete", "team-b2", "--delete-workspace"])
    assert result.exit_code == 0
    assert not workspace.exists()


