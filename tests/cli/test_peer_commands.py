import json

from hive.cli import cli
from hive import tmux


def test_peer_show_reports_implicit_pair_for_two_agent_team(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-p", "--workspace", str(workspace)]).exit_code == 0
    tmux.tag_pane("%99", "agent", "kiki", "team-p", cli="codex")

    result = runner.invoke(cli, ["peer", "show"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "team-p"
    assert payload["mode"] == "implicit"
    assert payload["pairs"] == [["kiki", "orch"]]


def test_peer_set_clear_and_team_output(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-peer", "--workspace", str(workspace)]).exit_code == 0
    tmux.tag_pane("%99", "agent", "kiki", "team-peer", cli="codex")
    tmux.tag_pane("%98", "agent", "momo", "team-peer", cli="claude")
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *args, **kwargs: 4321)
    monkeypatch.setattr(
        "hive.sidecar.request_team_runtime",
        lambda _ws, *, team: {"ok": True, "team": team, "members": {}},
    )

    result = runner.invoke(cli, ["peer", "set", "orch", "kiki"])
    assert result.exit_code == 0
    assert "Peer set: orch <-> kiki." in result.output

    show_result = runner.invoke(cli, ["peer", "show"])
    assert show_result.exit_code == 0
    show_payload = json.loads(show_result.output)
    assert show_payload["mode"] == "explicit"
    assert show_payload["pairs"] == [["kiki", "orch"]]

    team_result = runner.invoke(cli, ["team"])
    assert team_result.exit_code == 0
    team_payload = json.loads(team_result.output)
    orch = next(member for member in team_payload["members"] if member["name"] == "orch")
    kiki = next(member for member in team_payload["members"] if member["name"] == "kiki")
    momo = next(member for member in team_payload["members"] if member["name"] == "momo")
    assert orch["peer"] == "kiki"
    assert kiki["peer"] == "orch"
    assert "peer" not in momo

    clear_result = runner.invoke(cli, ["peer", "clear", "orch"])
    assert clear_result.exit_code == 0
    assert "Peer cleared: orch <-> kiki." in clear_result.output

    cleared_payload = json.loads(runner.invoke(cli, ["peer", "show"]).output)
    assert cleared_payload["mode"] == "none"
    assert "pairs" not in cleared_payload
