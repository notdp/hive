import json

from hive.cli import cli


def test_terminal_add_and_remove(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-t", "--workspace", str(workspace)]).exit_code == 0

    result = runner.invoke(cli, ["terminal", "add", "my-term", "-t", "team-t", "--pane", "%99"])
    assert result.exit_code == 0
    assert "my-term" in result.output

    config = json.loads((tmp_path / ".hive" / "teams" / "team-t" / "config.json").read_text())
    assert any(t["name"] == "my-term" for t in config["terminals"])

    result = runner.invoke(cli, ["terminal", "remove", "my-term", "-t", "team-t"])
    assert result.exit_code == 0

    config = json.loads((tmp_path / ".hive" / "teams" / "team-t" / "config.json").read_text())
    assert len(config["terminals"]) == 0


def test_exec_sends_to_terminal(runner, configure_hive_home, mock_tmux_send, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-e", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["terminal", "add", "shell", "-t", "team-e", "--pane", "%50"]).exit_code == 0

    result = runner.invoke(cli, ["exec", "shell", "htop", "-t", "team-e"])
    assert result.exit_code == 0
    assert any(text == "htop" for _, text in mock_tmux_send)
