import json
from types import SimpleNamespace

from hive.agent import Agent
from hive.cli import cli


def test_crew_init_creates_orch_and_challenger_without_board(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")

    import hive.cli as cli_mod

    pane_options: list[tuple[str, str, str]] = []
    spawned: list[dict[str, object]] = []
    selected_windows: list[str] = []

    repo = tmp_path / "repo"
    repo.mkdir()

    profile = SimpleNamespace(name="codex", skill_cmd="/{name}")
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _pane: profile)
    monkeypatch.setattr(cli_mod, "family_for_pane", lambda _pane: "openai")
    monkeypatch.setattr(cli_mod, "resolve_peer_spawn", lambda **_kwargs: ("claude", ""))
    monkeypatch.setattr(cli_mod.tmux, "get_current_window_index", lambda: "0")
    monkeypatch.setattr(cli_mod.tmux, "get_pane_count", lambda _pane: 1)
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda _target, _name: None)
    monkeypatch.setattr(
        cli_mod.tmux,
        "display_value",
        lambda _pane, fmt: "dev:0" if fmt == "#{session_name}:#{window_index}" else str(repo),
    )
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda p, k, v: pane_options.append((p, k, v)))
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(cli_mod.tmux, "send_key", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(cli_mod.tmux, "select_window", lambda target: selected_windows.append(target))
    monkeypatch.setattr(cli_mod.tmux, "configure_hive_window", lambda _target: None)
    monkeypatch.setattr("hive.team.tmux.configure_hive_window", lambda _target: None)
    monkeypatch.setattr("hive.sidecar.stop_sidecar", lambda _workspace: None)
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _target, _count: True)
    monkeypatch.setattr(
        "hive.layout.apply_adaptive",
        lambda _target: SimpleNamespace(orientation="horizontal"),
    )
    monkeypatch.setattr(cli_mod.time, "sleep", lambda _seconds: None)

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        name = str(kwargs["name"])
        team_name = str(kwargs["team_name"])
        cli_name = str(kwargs["cli"])
        pane_id = "%101"
        cli_mod.tmux.tag_pane(pane_id, "agent", name, team_name, cli=cli_name)
        return Agent(name=name, team_name=team_name, pane_id=pane_id, cli=cli_name)

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))

    result = runner.invoke(cli, ["crew", "init", "--name", "peaky"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["crewName"] == "peaky"
    assert payload["orch"]["name"] == "peaky.orch"
    assert payload["challenger"]["name"] == "peaky.challenger"
    assert payload["challenger"]["pane"] == "%101"
    assert payload["dispatched"] == ["peaky.orch", "peaky.challenger"]
    assert "board" not in payload
    assert selected_windows == ["dev:0"]

    assert len(spawned) == 1
    assert spawned[0]["name"] == "peaky.challenger"
    assert spawned[0]["split_horizontal"] is True
    assert not any(key == "hive-role" and value == "board" for _, key, value in pane_options)
