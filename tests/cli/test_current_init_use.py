import json

from hive.cli import cli


def test_teams_lists_known_teams(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()

    assert runner.invoke(cli, ["create", "team-a", "--workspace", str(tmp_path / "ws-a")]).exit_code == 0

    # Switch to a different window for the second team
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%1")

    assert runner.invoke(cli, ["create", "team-b", "--workspace", str(tmp_path / "ws-b")]).exit_code == 0

    result = runner.invoke(cli, ["teams"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert sorted(row["name"] for row in payload) == ["team-a", "team-b"]
    team_a = next(r for r in payload if r["name"] == "team-a")
    assert team_a["members"] == ["orch"]


def test_use_sets_current_context(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-c", "--workspace", str(workspace)]).exit_code == 0
    result = runner.invoke(cli, ["use", "team-c", "--agent", "claude"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {"team": "team-c", "workspace": str(workspace), "agent": "claude"}


def test_use_rejects_team_from_different_tmux_window(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(session_name="dev")

    # Create team dev-0 at window dev:0 (default)
    assert runner.invoke(cli, ["create", "dev-0", "--workspace", str(tmp_path / "ws")]).exit_code == 0

    # Switch current window to dev:1
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:1")

    result = runner.invoke(cli, ["use", "dev-0"])

    assert result.exit_code != 0
    assert "belongs to tmux window 'dev:0'" in result.output


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
            PaneInfo("%0", "[orch]", command="droid"),
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


def test_current_ignores_persisted_context_inside_tmux_when_window_is_unbound(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    ctx_dir = tmp_path / ".hive" / "contexts"
    ctx_dir.mkdir(parents=True, exist_ok=True)
    (ctx_dir / "default.json").write_text(json.dumps({"team": "stale-team", "workspace": "/tmp/ws", "agent": "claude"}))

    result = runner.invoke(cli, ["current"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] is None
    assert payload["hint"].startswith("No team bound")


def test_current_no_tmux_no_team(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["current"])

    assert result.exit_code != 0
    assert "requires tmux" in result.output


def test_current_discovers_registered_agent_from_tmux_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%9", session_name="dev")

    # Set up tmux state directly (no more config.json)
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "lead", "orch", "dev")
    tmux.tag_pane("%9", "agent", "alpha", "dev")

    result = runner.invoke(cli, ["current"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "team": "dev",
        "workspace": str(tmp_path / "ws"),
        "agent": "alpha",
        "role": "agent",
        "pane": "%9",
        "tmuxSession": "dev",
        "tmuxWindow": "dev:0",
    }


def test_current_shows_tagged_role_for_lead_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%0", session_name="dev")

    # Set up tmux state
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "lead", "orch", "dev")

    # Even when the pane command is a shell, role comes from tmux tag
    monkeypatch.setattr("hive.cli.tmux.get_pane_current_command", lambda _pane: "python3.12")

    result = runner.invoke(cli, ["current"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    # Role is now read from tmux pane tags, not dynamic command detection
    assert payload["role"] == "lead"


def test_current_returns_tagged_role_regardless_of_tty(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%0", session_name="dev")

    # Set up tmux state
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "lead", "orch", "dev")

    # These overrides don't affect the role (it comes from pane tags now)
    monkeypatch.setattr("hive.cli.tmux.get_pane_current_command", lambda _pane: "2.1.88")
    monkeypatch.setattr("hive.cli.tmux.get_pane_title", lambda _pane: "✳ Claude Code")
    monkeypatch.setattr("hive.cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.cli.tmux.list_tty_commands", lambda _tty: ["-zsh", "claude"])

    result = runner.invoke(cli, ["current"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    # Role is determined by tmux pane tags, not command/tty detection
    assert payload["role"] == "lead"


def test_init_returns_existing_team_for_registered_member(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%9", session_name="dev")

    # Set up tmux state directly (no more config.json)
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "lead", "orch", "dev")
    tmux.tag_pane("%9", "agent", "alpha", "dev")

    result = runner.invoke(cli, ["init"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "team": "dev",
        "workspace": str(tmp_path / "ws"),
        "agent": "alpha",
        "role": "agent",
        "pane": "%9",
        "tmuxSession": "dev",
        "tmuxWindow": "dev:0",
    }


def test_init_creates_team_registers_agents_and_notifies(runner, configure_hive_home, monkeypatch, mock_tmux_send, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr("hive.cli.secrets.choice", lambda names: "nini")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%5", "[orch]", command="droid"),
            PaneInfo("%6", "⛬ Claude", command="droid"),
            PaneInfo("%7", "", command="zsh"),
        ],
    )

    workspace = tmp_path / "ws"
    result = runner.invoke(cli, ["init", "--workspace", str(workspace)])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "dev-2"
    assert payload["workspace"] == str(workspace)
    assert len(payload["panes"]) == 3
    assert payload["panes"][0]["isSelf"] is True
    assert payload["panes"][0]["name"] == "orch"
    assert payload["panes"][0]["role"] == "agent"
    assert payload["panes"][1]["name"] == "nini"
    assert payload["panes"][1]["role"] == "agent"
    assert payload["panes"][2]["name"] == "term-1"
    assert payload["panes"][2]["role"] == "terminal"

    assert [text for _, text in mock_tmux_send if text == "/hive"] == ["/hive"]
    assert len([text for _, text in mock_tmux_send if "<HIVE ...>" in text]) == 1

    ctx_alpha = json.loads((tmp_path / ".hive" / "contexts" / "pane-6.json").read_text())
    assert ctx_alpha == {"team": "dev-2", "workspace": str(workspace), "agent": "nini"}
    current = json.loads((tmp_path / ".hive" / "contexts" / "default.json").read_text())
    assert current["team"] == "dev-2"
    assert current["agent"] == "orch"


def test_init_no_notify(runner, configure_hive_home, monkeypatch, mock_tmux_send, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "0")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%0")
    monkeypatch.setattr("hive.cli.secrets.choice", lambda names: "kiki")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%0", "", command="droid"), PaneInfo("%1", "GPT", command="droid")],
    )

    workspace = tmp_path / "ws"
    result = runner.invoke(cli, ["init", "--workspace", str(workspace), "--no-notify"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "dev-0"
    pane_map = {p["name"]: p["paneId"] for p in payload["panes"]}
    assert pane_map["kiki"] == "%1"
    assert mock_tmux_send == []


def test_init_excludes_names_already_used_in_current_window(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "0")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%0")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%0", "[orch]", command="droid"),
            PaneInfo("%1", "Claude", command="droid", agent="nini"),
            PaneInfo("%2", "GPT", command="droid", agent="kiki"),
        ],
    )

    seen_choices: list[list[str]] = []

    def fake_choice(names):
        seen_choices.append(list(names))
        return names[0]

    monkeypatch.setattr("hive.cli.secrets.choice", fake_choice)

    workspace = tmp_path / "ws"
    result = runner.invoke(cli, ["init", "--workspace", str(workspace), "--no-notify"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    peer_names = [pane["name"] for pane in payload["panes"] if not pane["isSelf"]]
    assert "nini" not in seen_choices[0]
    assert "kiki" not in seen_choices[0]
    assert "nini" not in peer_names
    assert "kiki" not in peer_names
    assert len(peer_names) == len(set(peer_names))


def test_init_resets_default_auto_workspace_before_reuse(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    auto_workspace = tmp_path / "auto-ws"
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda _session, _window: auto_workspace)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%5", "", command="droid"), PaneInfo("%6", "GPT", command="droid")],
    )

    (auto_workspace / "status").mkdir(parents=True, exist_ok=True)
    (auto_workspace / "artifacts").mkdir(parents=True, exist_ok=True)
    (auto_workspace / "status" / "orch.json").write_text(json.dumps({"state": "done"}))
    (auto_workspace / "artifacts" / "stale.txt").write_text("stale")

    result = runner.invoke(cli, ["init", "--no-notify"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["workspace"] == str(auto_workspace)
    assert list((auto_workspace / "status").iterdir()) == []
    assert list((auto_workspace / "artifacts").iterdir()) == []


def test_init_with_explicit_workspace_does_not_reset_existing_managed_dirs(
    runner, configure_hive_home, monkeypatch, tmp_path,
):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%5", "", command="droid"), PaneInfo("%6", "GPT", command="droid")],
    )

    workspace = tmp_path / "custom-ws"
    (workspace / "status").mkdir(parents=True, exist_ok=True)
    stale = workspace / "status" / "orch.json"
    stale.write_text(json.dumps({"state": "done"}))

    result = runner.invoke(cli, ["init", "--workspace", str(workspace), "--no-notify"])

    assert result.exit_code == 0
    assert stale.exists()


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


def test_current_gc_removes_leftover_team_dir_for_dead_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%8", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%8")

    from hive.tmux import PaneInfo

    monkeypatch.setattr("hive.cli.tmux.list_panes_full", lambda _target: [PaneInfo("%8", "", command="droid")])

    # Leftover team dir from a dead team (no corresponding tmux window)
    team_dir = tmp_path / ".hive" / "teams" / "dev-0"
    team_dir.mkdir(parents=True)

    result = runner.invoke(cli, ["current"])

    assert result.exit_code == 0
    # GC removes leftover team dirs not backed by live tmux windows
    assert not team_dir.exists()


def test_init_uses_window_scoped_default_team_name_when_same_session_has_other_team(
    runner, configure_hive_home, monkeypatch, tmp_path,
):
    configure_hive_home(current_pane="%8", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "1")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%8")

    from hive.tmux import PaneInfo

    # Set up existing team "dev-0" in tmux state at window dev:0
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev-0")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws-0"))

    monkeypatch.setattr("hive.cli.tmux.list_panes_full", lambda _target: [PaneInfo("%8", "", command="droid")])

    result = runner.invoke(cli, ["init", "--workspace", str(tmp_path / "ws-1"), "--no-notify"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "dev-1"


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
    monkeypatch.setattr("hive.cli.secrets.choice", lambda names: "dodo")

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
    assert payload["team"] == "dev-0"
    roles = {p["name"]: p["role"] for p in payload["panes"]}
    assert roles["orch"] == "agent"
    assert roles["dodo"] == "agent"
    assert roles["term-1"] == "terminal"
    assert roles["term-2"] == "terminal"



def test_legacy_commands_removed(runner):
    for command in ("comment", "wait", "read", "inbox"):
        result = runner.invoke(cli, [command, "--help"])
        assert result.exit_code != 0
        assert f"No such command '{command}'" in result.output


def test_root_help_groups_commands_by_area(runner):
    result = runner.invoke(cli, ["--help"])

    assert result.exit_code == 0
    assert "Hive - tmux-first multi-agent collaboration runtime." in result.output
    assert "Context:" in result.output
    assert "Team Setup:" in result.output
    assert "Communication:" in result.output
    assert "Pane Control:" in result.output
    assert "Extensions:" in result.output
    assert "User Attention:" in result.output
    assert "Examples:" in result.output
    assert "hive init" in result.output
    assert "team   Show team overview." in result.output
    assert "status       Show published statuses only." in result.output
    assert "inject     Debug: inject raw input into an agent pane." in result.output
    assert "plugin  Manage first-party Hive plugins." in result.output
    assert "who" not in result.output
    assert "statuses     " not in result.output
    assert "status-show" not in result.output
    assert "type" not in result.output
    assert "current  " not in result.output
