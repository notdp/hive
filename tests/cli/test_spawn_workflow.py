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
            "--color",
            "cyan",
            "--cwd",
            "/tmp/project",
            "--skill",
            "none",
            "--workflow",
            "cross-review",
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
        "color": "cyan",
        "cwd": "/tmp/project",
        "skill": "none",
        "workflow": "cross-review",
        "extra_env": {"CR_WORKSPACE": "/tmp/cr", "MODE": "test"},
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


def test_workflow_load_loads_skill_and_optional_prompt(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    calls: list[str] = []

    class _FakeAgent:
        def load_skill(self, skill_name: str) -> None:
            calls.append(f"skill:{skill_name}")

        def send(self, text: str) -> None:
            calls.append(f"send:{text}")

    class _FakeTeam:
        name = "team-x"
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(
        cli,
        ["workflow", "load", "claude", "cross-review", "--prompt", "start now"],
    )

    assert result.exit_code == 0
    assert calls == ["skill:cross-review", "send:start now"]
    assert "Workflow 'cross-review' loaded into claude." in result.output
