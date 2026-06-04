import json
from types import SimpleNamespace

from hive.agent import Agent
from hive.cli import cli, _attach_cell_to_team as _real_attach_cell_to_team


def _cell_mocks(cli_mod, monkeypatch, repo, *, pane_count, family_map=None, panes=None):
    """Shared cell-init stubs: codex worker (openai family), claude validator."""
    # configure_hive_home stubs _attach_cell_to_team; restore the real one so
    # these tests exercise the actual cell-formation logic.
    monkeypatch.setattr(cli_mod, "_attach_cell_to_team", _real_attach_cell_to_team)
    profile = SimpleNamespace(name="codex", skill_cmd="/{name}")
    fam = family_map or {}
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _p: profile)
    monkeypatch.setattr(cli_mod, "family_for_pane", lambda p: fam.get(p, "openai"))
    monkeypatch.setattr(cli_mod, "resolve_peer_spawn", lambda **_k: ("claude", ""))
    monkeypatch.setattr(cli_mod, "anti_peer_cli", lambda _c: "claude")
    monkeypatch.setattr(cli_mod, "_require_codex_daemon_backed", lambda _p: None)
    monkeypatch.setattr(cli_mod, "_resolve_spawn_cli_name", lambda _a: "codex")
    monkeypatch.setattr(cli_mod, "_pane_is_idle_for_pairing", lambda _p: True)
    monkeypatch.setattr(cli_mod.tmux, "get_pane_count", lambda _p: pane_count)
    monkeypatch.setattr(cli_mod.tmux, "get_pane_window_target", lambda _p: "dev:0")
    if panes is not None:
        monkeypatch.setattr(cli_mod.tmux, "list_panes_full", lambda _w: panes)
    monkeypatch.setattr(
        cli_mod.tmux,
        "display_value",
        lambda _p, fmt: "dev:0" if fmt == "#{session_name}:#{window_index}" else str(repo),
    )
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "set_window_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "configure_hive_window", lambda _t: None)
    monkeypatch.setattr("hive.team.tmux.configure_hive_window", lambda _t: None)
    monkeypatch.setattr(cli_mod.tmux, "select_window", lambda _t: None)
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda *_a, **_k: None)
    monkeypatch.setattr(cli_mod.tmux, "send_key", lambda *_a, **_k: None)
    monkeypatch.setattr("hive.sidecar.stop_sidecar", lambda _ws: None)
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _t, _c: True)
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda _t: SimpleNamespace(orientation="horizontal"))


def test_cell_init_one_pane_spawns_antifamily_validator(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    _cell_mocks(cli_mod, monkeypatch, repo, pane_count=1)

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id="%101",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))

    result = runner.invoke(cli, ["cell", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["group"] == "cell"
    assert payload["worker"] == {"pane": "%100", "name": "worker", "cli": "codex"}
    assert payload["validator"]["name"] == "validator"
    assert payload["validator"]["mode"] == "spawned"
    assert payload["validator"]["pane"] == "%101"
    assert payload["validator"]["cli"] == "claude"  # anti-family of codex worker

    assert len(spawned) == 1
    assert spawned[0]["name"] == "validator"
    assert spawned[0]["cli"] == "claude"
    assert spawned[0]["skill"] == "none"
    # validator's first message is the fed role bootstrap (preamble + spec), not a bare command,
    # so a spawned no-human pane operates on it directly instead of run-and-stop.
    assert "cell-validator" in spawned[0]["prompt"]
    assert "等你的第一条任务消息" in spawned[0]["prompt"]
    assert payload["dispatched"] == ["worker", "validator"]


def test_cell_init_two_panes_adopts_idle_antifamily_neighbor(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    breaks: list[str] = []
    panes = [
        SimpleNamespace(pane_id="%100", team="", group=""),
        SimpleNamespace(pane_id="%150", team="", group=""),
    ]
    _cell_mocks(
        cli_mod,
        monkeypatch,
        repo,
        pane_count=2,
        family_map={"%100": "openai", "%150": "anthropic"},
        panes=panes,
    )
    monkeypatch.setattr(
        cli_mod.Agent,
        "spawn",
        staticmethod(lambda **k: spawned.append(k) or Agent(name="x", team_name="t", pane_id="%999", cli="claude")),
    )
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: breaks.append(p) or ("dev:1", "%200"))

    result = runner.invoke(cli, ["cell", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["validator"]["mode"] == "paired"
    assert payload["validator"]["pane"] == "%150"
    assert payload["worker"]["pane"] == "%100"  # stays put, not broken out
    assert spawned == []  # adopted the neighbor, did not spawn
    assert breaks == []  # 2-pane pairable → no break-out
    assert payload["dispatched"] == ["worker", "validator"]  # worker + adopted validator both get their role


def test_cell_init_two_panes_same_family_breaks_out_then_spawns(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """A 2-pane window whose neighbor is same-family is not pairable, so the
    worker breaks out to a clean window and the validator is spawned there."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    breaks: list[str] = []
    panes = [
        SimpleNamespace(pane_id="%100", team="", group=""),
        SimpleNamespace(pane_id="%150", team="", group=""),
    ]
    _cell_mocks(
        cli_mod,
        monkeypatch,
        repo,
        pane_count=2,
        family_map={"%100": "openai", "%150": "openai"},  # same family → not pairable
        panes=panes,
    )

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id="%101",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: breaks.append(p) or ("dev:1", "%200"))

    result = runner.invoke(cli, ["cell", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert breaks == ["%100"]  # unpairable → worker broken out
    assert payload["worker"]["pane"] == "%200"
    assert payload["window"] == "dev:1"
    assert payload["validator"]["mode"] == "spawned"
    assert len(spawned) == 1
    assert payload["dispatched"] == ["worker", "validator"]


def test_cell_init_three_panes_breaks_out_then_spawns(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    breaks: list[str] = []
    _cell_mocks(cli_mod, monkeypatch, repo, pane_count=3)

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id="%101",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: breaks.append(p) or ("dev:1", "%200"))

    result = runner.invoke(cli, ["cell", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert breaks == ["%100"]  # crowded window → break the worker out
    assert payload["worker"]["pane"] == "%200"
    assert payload["window"] == "dev:1"
    assert payload["validator"]["mode"] == "spawned"
    assert len(spawned) == 1
    assert payload["dispatched"] == ["worker", "validator"]
