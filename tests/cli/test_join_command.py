"""hive join: a session or pane enters a team's roster."""
import json
from types import SimpleNamespace

from hive.cli import cli


def _outside(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)


def _self_session(monkeypatch, session_id="ccd-sid-1", name="my-ccd"):
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.self_session",
        lambda: SimpleNamespace(session_id=session_id, name=name),
    )


def test_ccd_join_records_a_full_member(runner, configure_hive_home, monkeypatch):
    from hive import registry

    _outside(configure_hive_home, monkeypatch)
    _self_session(monkeypatch)
    assert registry.record_team(team="honey", workspace="", created_at="1.0") == "written"

    result = runner.invoke(cli, ["join", "honey", "--as", "scout"])

    assert result.exit_code == 0, result.output
    assert "joined: honey.scout" in result.output
    rows = registry.load("honey")["members"]
    assert rows == [{
        "name": "scout", "cli": "claude", "model": "",
        "sessionId": "ccd-sid-1", "cwd": rows[0]["cwd"],
    }]


def test_ccd_join_is_idempotent(runner, configure_hive_home, monkeypatch):
    from hive import registry

    _outside(configure_hive_home, monkeypatch)
    _self_session(monkeypatch)
    registry.record_team(team="honey", workspace="", created_at="1.0")
    assert runner.invoke(cli, ["join", "honey", "--as", "scout"]).exit_code == 0

    result = runner.invoke(cli, ["join", "honey"])

    assert result.exit_code == 0, result.output
    assert "already a member: honey.scout" in result.output
    assert len(registry.load("honey")["members"]) == 1


def test_ccd_join_refuses_a_second_team(runner, configure_hive_home, monkeypatch):
    from hive import registry

    _outside(configure_hive_home, monkeypatch)
    _self_session(monkeypatch)
    registry.record_team(team="honey", workspace="", created_at="1.0")
    registry.record_team(team="wasp", workspace="", created_at="2.0")
    assert runner.invoke(cli, ["join", "honey", "--as", "scout"]).exit_code == 0

    result = runner.invoke(cli, ["join", "wasp"])

    assert result.exit_code != 0
    assert "already honey.scout" in result.output


def test_ccd_join_needs_a_team(runner, configure_hive_home, monkeypatch):
    _outside(configure_hive_home, monkeypatch)
    result = runner.invoke(cli, ["join"])
    assert result.exit_code != 0
    assert "needs a team" in result.output


def test_ccd_join_unknown_team_fails(runner, configure_hive_home, monkeypatch):
    _outside(configure_hive_home, monkeypatch)
    _self_session(monkeypatch)
    result = runner.invoke(cli, ["join", "nosuch"])
    assert result.exit_code != 0
    assert "not found" in result.output


def test_ccd_join_without_a_session_channel_fails(runner, configure_hive_home, monkeypatch):
    from hive import registry

    _outside(configure_hive_home, monkeypatch)
    registry.record_team(team="honey", workspace="", created_at="1.0")
    monkeypatch.setattr("hive.adapters.claude_sessions.self_session", lambda: None)

    result = runner.invoke(cli, ["join", "honey"])

    assert result.exit_code != 0
    assert "session channel" in result.output


def test_headless_create_seats_the_creator_as_orch(runner, configure_hive_home, monkeypatch):
    from hive import registry

    _outside(configure_hive_home, monkeypatch)
    _self_session(monkeypatch, session_id="ccd-sid-9")

    result = runner.invoke(cli, ["create", "honey"])

    assert result.exit_code == 0, result.output
    assert "You are honey.orch" in result.output
    rows = registry.load("honey")["members"]
    assert [(m["name"], m["cli"], m["sessionId"]) for m in rows] == [("orch", "claude", "ccd-sid-9")]


def test_headless_create_leaves_a_foreign_member_as_guest(runner, configure_hive_home, monkeypatch):
    from hive import registry

    _outside(configure_hive_home, monkeypatch)
    _self_session(monkeypatch, session_id="ccd-sid-9")
    registry.record_team(team="wasp", workspace="", created_at="1.0")
    assert runner.invoke(cli, ["join", "wasp", "--as", "scout"]).exit_code == 0

    result = runner.invoke(cli, ["create", "honey"])

    assert result.exit_code == 0, result.output
    assert "already wasp.scout" in result.output and "guest" in result.output
    assert registry.load("honey")["members"] == []
