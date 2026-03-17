import json

from hive.cli import cli


def test_teams_lists_known_teams(runner, configure_hive_home, tmp_path):
    configure_hive_home()

    assert runner.invoke(cli, ["create", "team-a", "--workspace", str(tmp_path / "ws-a")]).exit_code == 0
    assert runner.invoke(cli, ["create", "team-b", "--workspace", str(tmp_path / "ws-b")]).exit_code == 0

    result = runner.invoke(cli, ["teams"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert [row["name"] for row in payload] == ["team-a", "team-b"]
    assert payload[0]["members"] == ["orchestrator"]


def test_use_sets_current_context(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-c", "--workspace", str(workspace)]).exit_code == 0
    result = runner.invoke(cli, ["use", "team-c", "--agent", "claude"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {"team": "team-c", "workspace": str(workspace), "agent": "claude"}


def test_current_reads_persisted_context(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-d", "--workspace", str(workspace)]).exit_code == 0
    result = runner.invoke(cli, ["current"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "team-d"
    assert payload["workspace"] == str(workspace)


def test_current_discovers_tmux_when_no_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "main")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "main:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%0")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%0", "[orchestrator]", command="droid"),
            PaneInfo("%12", "[claude]", command="droid"),
        ],
    )

    result = runner.invoke(cli, ["current"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] is None
    assert payload["tmux"]["session"] == "main"
    assert payload["tmux"]["paneCount"] == 2
    assert payload["tmux"]["panes"][0]["id"] == "%0"
    assert payload["tmux"]["panes"][0]["role"] == "agent"
    assert payload["tmux"]["panes"][1]["role"] == "agent"
    assert "hive init" in payload["hint"]


def test_current_no_tmux_no_team(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["current"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] is None
    assert payload["tmux"] is None
    assert "tmux" in payload["hint"]


def test_current_discovers_registered_agent_from_tmux_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%9", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%9")

    team_dir = tmp_path / ".hive" / "teams" / "dev"
    team_dir.mkdir(parents=True)
    (team_dir / "config.json").write_text(json.dumps({
        "name": "dev",
        "description": "",
        "workspace": str(tmp_path / "ws"),
        "leadName": "orchestrator",
        "leadPaneId": "%0",
        "leadSessionId": None,
        "tmuxSession": "dev",
        "createdAt": 0,
        "members": [
            {"name": "alpha", "tmuxPaneId": "%9", "model": "", "prompt": "", "color": "green", "cwd": "", "sessionId": None, "spawnedAt": 0},
        ],
        "terminals": [],
    }))

    result = runner.invoke(cli, ["current"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {"team": "dev", "workspace": str(tmp_path / "ws"), "agent": "alpha"}


def test_init_returns_existing_team_for_registered_member(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%9", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%9")

    team_dir = tmp_path / ".hive" / "teams" / "dev"
    team_dir.mkdir(parents=True)
    (team_dir / "config.json").write_text(json.dumps({
        "name": "dev",
        "description": "",
        "workspace": str(tmp_path / "ws"),
        "leadName": "orchestrator",
        "leadPaneId": "%0",
        "leadSessionId": None,
        "tmuxSession": "dev",
        "createdAt": 0,
        "members": [
            {"name": "alpha", "tmuxPaneId": "%9", "model": "", "prompt": "", "color": "green", "cwd": "", "sessionId": None, "spawnedAt": 0},
        ],
        "terminals": [],
    }))

    result = runner.invoke(cli, ["init"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {"team": "dev", "workspace": str(tmp_path / "ws"), "agent": "alpha"}


def test_init_creates_team_registers_agents_and_notifies(runner, configure_hive_home, monkeypatch, mock_tmux_send, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%5", "[orchestrator]", command="droid"),
            PaneInfo("%6", "⛬ Claude", command="droid"),
            PaneInfo("%7", "", command="zsh"),
        ],
    )

    workspace = tmp_path / "ws"
    result = runner.invoke(cli, ["init", "--workspace", str(workspace)])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "dev"
    assert payload["workspace"] == str(workspace)
    assert len(payload["panes"]) == 3
    assert payload["panes"][0]["isSelf"] is True
    assert payload["panes"][0]["name"] == "orchestrator"
    assert payload["panes"][0]["role"] == "lead"
    assert payload["panes"][1]["name"] == "alpha"
    assert payload["panes"][1]["role"] == "agent"
    assert payload["panes"][2]["name"] == "term-1"
    assert payload["panes"][2]["role"] == "terminal"

    config = json.loads((tmp_path / ".hive" / "teams" / "dev" / "config.json").read_text())
    assert [m["name"] for m in config["members"]] == ["alpha"]
    assert [t["name"] for t in config["terminals"]] == ["term-1"]
    assert [text for _, text in mock_tmux_send if text == "/skill hive"] == ["/skill hive"]
    assert len([text for _, text in mock_tmux_send if "<HIVE ...>" in text]) == 1

    ctx_alpha = json.loads((tmp_path / ".hive" / "contexts" / "pane-6.json").read_text())
    assert ctx_alpha == {"team": "dev", "workspace": str(workspace), "agent": "alpha"}
    current = json.loads((tmp_path / ".hive" / "contexts" / "default.json").read_text())
    assert current["team"] == "dev"


def test_init_no_notify(runner, configure_hive_home, monkeypatch, mock_tmux_send, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "0")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%0")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%0", "", command="droid"), PaneInfo("%1", "GPT", command="droid")],
    )

    workspace = tmp_path / "ws"
    result = runner.invoke(cli, ["init", "--workspace", str(workspace), "--no-notify"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    pane_map = {p["name"]: p["paneId"] for p in payload["panes"]}
    assert pane_map["alpha"] == "%1"
    assert mock_tmux_send == []


def test_init_custom_name(runner, configure_hive_home, monkeypatch, mock_tmux_send, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "0")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%0")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%0", "", command="droid")],
    )

    workspace = tmp_path / "ws2"
    result = runner.invoke(cli, ["init", "--name", "my-team", "--workspace", str(workspace)])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "my-team"


def test_init_fails_outside_tmux(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["init"])
    assert result.exit_code != 0
    assert "tmux" in result.output.lower()


def test_init_classifies_terminals(runner, configure_hive_home, monkeypatch, mock_tmux_send, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "0")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%10")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%10", "orch", command="droid"),
            PaneInfo("%11", "Claude", command="droid"),
            PaneInfo("%12", "myshell", command="bash"),
            PaneInfo("%13", "fish", command="fish"),
        ],
    )

    ws = tmp_path / "ws"
    result = runner.invoke(cli, ["init", "--workspace", str(ws)])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    roles = {p["name"]: p["role"] for p in payload["panes"]}
    assert roles["orchestrator"] == "lead"
    assert roles["alpha"] == "agent"
    assert roles["term-1"] == "terminal"
    assert roles["term-2"] == "terminal"

    config = json.loads((tmp_path / ".hive" / "teams" / "dev" / "config.json").read_text())
    assert len(config["members"]) == 1
    assert len(config["terminals"]) == 2


def test_legacy_commands_removed(runner):
    for command in ("comment", "wait", "read", "inbox"):
        result = runner.invoke(cli, [command, "--help"])
        assert result.exit_code != 0
        assert f"No such command '{command}'" in result.output
