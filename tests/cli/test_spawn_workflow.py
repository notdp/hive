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

        def spawn(self, name: str, **kwargs):
            calls["name"] = name
            calls.update(kwargs)
            return _Spawned()

    monkeypatch.setattr("hive.cli._load_team", lambda _team: _FakeTeam())

    result = runner.invoke(
        cli,
        [
            "spawn",
            "claude",
            "--team",
            "team-x",
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


def test_spawn_writes_context_for_new_agent(runner, monkeypatch, tmp_path):
    hive_home = tmp_path / ".hive"
    workspace = tmp_path / "ws"
    workspace.mkdir(parents=True, exist_ok=True)

    class _Spawned:
        pane_id = "%99"

    class _FakeTeam:
        def __init__(self):
            self.name = "team-x"
            self.workspace = str(workspace)

        def spawn(self, *args, **kwargs):
            return _Spawned()

    monkeypatch.setattr("hive.context.HIVE_HOME", hive_home)
    monkeypatch.setattr("hive.context.CONTEXT_DIR", hive_home / "contexts")
    monkeypatch.setattr("hive.context.CURRENT_CONTEXT_FILE", hive_home / "current.json")
    monkeypatch.delenv("TMUX_PANE", raising=False)
    monkeypatch.setattr("hive.cli._load_team", lambda _team: _FakeTeam())

    result = runner.invoke(cli, ["spawn", "luxun-fan", "--team", "team-x"])
    assert result.exit_code == 0

    payload = json.loads((hive_home / "contexts" / "pane-99.json").read_text())
    assert payload == {"team": "team-x", "workspace": str(workspace), "agent": "luxun-fan"}


def test_workflow_load_loads_skill_and_optional_prompt(runner, monkeypatch):
    calls: list[str] = []

    class _FakeAgent:
        def load_skill(self, skill_name: str) -> None:
            calls.append(f"skill:{skill_name}")

        def send(self, text: str) -> None:
            calls.append(f"send:{text}")

    class _FakeTeam:
        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._load_team", lambda _team: _FakeTeam())

    result = runner.invoke(
        cli,
        ["workflow", "load", "claude", "cross-review", "--team", "team-x", "--prompt", "start now"],
    )

    assert result.exit_code == 0
    assert calls == ["skill:cross-review", "send:start now"]
    assert "Workflow 'cross-review' loaded into claude." in result.output
