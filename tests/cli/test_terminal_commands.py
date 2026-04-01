import json

from hive.cli import cli


def test_terminal_add_and_remove(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-t", "--workspace", str(workspace)]).exit_code == 0

    result = runner.invoke(cli, ["terminal", "add", "my-term", "--pane", "%99"])
    assert result.exit_code == 0
    assert "my-term" in result.output

    # Verify terminal was added via Team.load (no more config.json)
    from hive.team import Team
    team = Team.load("team-t")
    assert "my-term" in team.terminals

    result = runner.invoke(cli, ["terminal", "remove", "my-term"])
    assert result.exit_code == 0

    # After remove, pane tags are cleared so Team.load won't find the terminal
    team = Team.load("team-t")
    assert "my-term" not in team.terminals


def test_exec_sends_to_terminal(runner, configure_hive_home, mock_tmux_send, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-e", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["terminal", "add", "shell", "--pane", "%50"]).exit_code == 0

    result = runner.invoke(cli, ["exec", "shell", "htop"])
    assert result.exit_code == 0
    assert any(text == "htop" for _, text in mock_tmux_send)


def test_terminal_add_rejects_pane_from_different_window(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(session_name="dev")
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-t", "--workspace", str(workspace)]).exit_code == 0
    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:1")

    result = runner.invoke(cli, ["terminal", "add", "my-term", "--pane", "%99"])

    assert result.exit_code != 0
    assert "not team 'team-t' window 'dev:0'" in result.output
