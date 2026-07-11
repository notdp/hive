import json

from hive.cli import cli


def test_spawn_forwards_cli_options_to_team(runner, configure_hive_home, monkeypatch, tmp_path):
    hive_home = configure_hive_home()
    calls: dict[str, object] = {}
    workspace = tmp_path / "ws"
    workspace.mkdir()

    class _Spawned:
        pane_id = "%55"

    class _FakeTeam:
        def __init__(self):
            self.name = "team-x"
            self.workspace = str(workspace)
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def spawn(self, name: str, **kwargs):
            calls["name"] = name
            calls.update(kwargs)
            return _Spawned()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(
        cli,
        [
            "spawn",
            "claude",
            "--model",
            "custom:Claude-Opus-4.6-0",
            "--prompt",
            "hello",
            "--cwd",
            "/tmp/project",
            "--skill",
            "none",
            "--workflow",
            "demo-review",
            "--env",
            "CR_WORKSPACE=/tmp/cr",
            "--env",
            "MODE=test",
        ],
    )

    assert result.exit_code == 0
    assert calls == {
        "name": "claude",
        "model": "custom:Claude-Opus-4.6-0",
        "prompt": "hello",
        "cwd": "/tmp/project",
        "skill": "none",
        "workflow": "demo-review",
        "extra_env": {"CR_WORKSPACE": "/tmp/cr", "MODE": "test"},
        "cli": "claude",
    }
    payload = json.loads((hive_home / "contexts" / "pane-55.json").read_text())
    assert payload == {"team": "team-x", "workspace": str(workspace), "agent": "claude"}


def test_spawn_writes_context_for_new_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    hive_home = configure_hive_home()
    workspace = tmp_path / "ws"
    workspace.mkdir(parents=True, exist_ok=True)

    class _Spawned:
        pane_id = "%99"

    class _FakeTeam:
        def __init__(self):
            self.name = "team-x"
            self.workspace = str(workspace)
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def spawn(self, *args, **kwargs):
            return _Spawned()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["spawn", "luxun-fan"])
    assert result.exit_code == 0

    payload = json.loads((hive_home / "contexts" / "pane-99.json").read_text())
    assert payload == {"team": "team-x", "workspace": str(workspace), "agent": "luxun-fan"}


def test_workflow_group_is_removed(runner):
    """`hive workflow load` was the last send-keys-era skill injector; the
    whole group is gone and invoking it fails like any unknown command."""
    result = runner.invoke(cli, ["workflow", "load", "claude", "demo-review"])
    assert result.exit_code == 2
    assert "No such command 'workflow'" in result.output
