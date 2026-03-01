import json
from pathlib import Path

from click.testing import CliRunner

from hive.cli import cli


def _set_hive_home(monkeypatch, tmp_path: Path) -> Path:
    hive_home = tmp_path / ".hive"
    monkeypatch.setattr("hive.team.HIVE_HOME", hive_home)
    monkeypatch.setattr("hive.cli.HIVE_HOME", hive_home)
    monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.team.tmux.get_current_pane_id", lambda: "%0")
    return hive_home


def test_create_initializes_workspace_and_state(monkeypatch, tmp_path):
    hive_home = _set_hive_home(monkeypatch, tmp_path)
    runner = CliRunner()
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

    config = json.loads((hive_home / "teams" / "team-a" / "config.json").read_text())
    assert config["workspace"] == str(workspace)


def test_delete_removes_workspace(monkeypatch, tmp_path):
    _set_hive_home(monkeypatch, tmp_path)
    runner = CliRunner()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-b", "--workspace", str(workspace)]).exit_code == 0
    (workspace / "results").mkdir(parents=True, exist_ok=True)
    (workspace / "results" / "x.txt").write_text("ok")

    result = runner.invoke(cli, ["delete", "team-b"])
    assert result.exit_code == 0
    assert not workspace.exists()


def test_wait_succeeds_on_sentinel(tmp_path):
    runner = CliRunner()
    workspace = tmp_path / "ws"
    (workspace / "results").mkdir(parents=True, exist_ok=True)
    (workspace / "results" / "claude-r1.done").write_text("")
    (workspace / "results" / "claude-r1.md").write_text("line1\nline2\n")

    result = runner.invoke(
        cli,
        ["wait", "claude", "r1", "--workspace", str(workspace), "--timeout", "1"],
    )
    assert result.exit_code == 0
    assert "DONE" in result.output


def test_wait_fails_when_agent_dead(monkeypatch, tmp_path):
    runner = CliRunner()
    workspace = tmp_path / "ws"
    (workspace / "results").mkdir(parents=True, exist_ok=True)

    class _FakeAgent:
        def capture(self, _lines: int) -> str:
            return "dead"

    class _FakeTeam:
        workspace = ""

        def status(self):
            return {"agents": {"claude": {"alive": False}}}

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._load_team", lambda _team: _FakeTeam())

    result = runner.invoke(
        cli,
        ["wait", "claude", "r1", "-t", "team-x", "--workspace", str(workspace), "--timeout", "2"],
    )
    assert result.exit_code != 0
    assert "no longer alive" in result.output


def test_comment_post_reads_workspace_state(monkeypatch, tmp_path):
    runner = CliRunner()
    workspace = tmp_path / "ws"
    (workspace / "state").mkdir(parents=True, exist_ok=True)
    (workspace / "state" / "repo").write_text("owner/repo")
    (workspace / "state" / "pr-number").write_text("7")
    (workspace / "state" / "pr-node-id").write_text("PR_node")

    def _fake_gh(args, input_text=None):
        assert args[0:3] == ["api", "graphql", "-f"]
        assert input_text is None
        return json.dumps({"data": {"addComment": {"commentEdge": {"node": {"id": "C_1"}}}}})

    monkeypatch.setattr("hive.cli._gh", _fake_gh)
    result = runner.invoke(cli, ["comment", "post", "hello", "--workspace", str(workspace)])
    assert result.exit_code == 0
    assert result.output.strip() == "C_1"


def test_comment_list_filters_cr_markers(monkeypatch, tmp_path):
    runner = CliRunner()
    workspace = tmp_path / "ws"
    (workspace / "state").mkdir(parents=True, exist_ok=True)
    (workspace / "state" / "repo").write_text("owner/repo")
    (workspace / "state" / "pr-number").write_text("7")

    def _fake_gh(args, input_text=None):
        assert args[0:2] == ["pr", "view"]
        assert input_text is None
        return json.dumps(
            {
                "comments": [
                    {"id": "A", "body": "<!-- cr-summary -->\nbody", "createdAt": "t1"},
                    {"id": "B", "body": "normal", "createdAt": "t2"},
                ]
            }
        )

    monkeypatch.setattr("hive.cli._gh", _fake_gh)
    result = runner.invoke(cli, ["comment", "list", "--workspace", str(workspace)])
    assert result.exit_code == 0
    rows = json.loads(result.output)
    assert rows == [{"id": "A", "marker": "cr-summary", "createdAt": "t1"}]
