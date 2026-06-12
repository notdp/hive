import json
from types import SimpleNamespace

from hive.agent import Agent
from hive.cli import cli


def test_squad_init_creates_orch_and_challenger_without_board(
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
    sent: list = []
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda pane, text, **_k: sent.append((pane, text)))
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

    result = runner.invoke(cli, ["squad", "init", "--name", "peaky"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["squadName"] == "peaky"
    assert payload["orch"]["name"] == "peaky.orch"
    assert payload["challenger"]["name"] == "peaky.challenger"
    assert payload["challenger"]["pane"] == "%101"
    assert payload["dispatched"] == ["peaky.challenger"]
    assert payload["next"] == "hive skills get squad-orch"
    # The orch runs init itself: nothing may be injected into its pane.
    orch_pane = payload["orch"]["pane"]
    assert not [c for c in sent if c[0] == orch_pane and "skills get" in c[1]]
    # Positive control: the spawned challenger gets its role via launch prompt.
    assert spawned[0]["prompt"] == cli_mod._role_bootstrap_prompt("squad-challenger")
    assert "board" not in payload
    assert selected_windows == ["dev:0"]

    assert len(spawned) == 1
    assert spawned[0]["name"] == "peaky.challenger"
    assert spawned[0]["split_horizontal"] is True
    assert not any(key == "hive-role" and value == "board" for _, key, value in pane_options)


def test_squad_init_breakout_names_main_team_from_final_window_keeps_readable_squad(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Bug A (squad): after break-out the internal main team name follows the
    FINAL squad window's stable id, while the squad-facing namespace stays
    human-readable (peaky.orch / peaky.challenger)."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    spawned: list[dict[str, object]] = []
    breaks: list[str] = []
    repo = tmp_path / "repo"
    repo.mkdir()

    profile = SimpleNamespace(name="codex", skill_cmd="/{name}")
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _pane: profile)
    monkeypatch.setattr(cli_mod, "family_for_pane", lambda _pane: "openai")
    monkeypatch.setattr(cli_mod, "resolve_peer_spawn", lambda **_kwargs: ("claude", ""))
    monkeypatch.setattr(cli_mod.tmux, "get_current_window_index", lambda: "8")
    # Crowded origin → squad breaks out to dev:8 whose id slug (@88) differs from
    # its index (8), proving the main team name is id-derived, not index-derived.
    monkeypatch.setattr(cli_mod.tmux, "get_pane_count", lambda _pane: 2)
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: breaks.append(p) or ("dev:8", "%100"))
    monkeypatch.setattr(cli_mod.tmux, "get_window_id", lambda target: "@88" if target == "dev:8" else "@0")
    monkeypatch.setattr(
        cli_mod.tmux,
        "display_value",
        lambda _pane, fmt: "dev:0" if fmt == "#{session_name}:#{window_index}" else str(repo),
    )
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "set_window_option", lambda *_a: None)
    sent: list = []
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda pane, text, **_k: sent.append((pane, text)))
    monkeypatch.setattr(cli_mod.tmux, "send_key", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(cli_mod.tmux, "select_window", lambda _target: None)
    monkeypatch.setattr(cli_mod.tmux, "configure_hive_window", lambda _target: None)
    monkeypatch.setattr("hive.team.tmux.configure_hive_window", lambda _target: None)
    monkeypatch.setattr("hive.sidecar.stop_sidecar", lambda _workspace: None)
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _target, _count: True)
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda _target: SimpleNamespace(orientation="horizontal"))
    monkeypatch.setattr(cli_mod.time, "sleep", lambda _seconds: None)

    sidecar_calls: list[tuple[str, str, str, str]] = []
    monkeypatch.setattr(
        "hive.sidecar.ensure_sidecar",
        lambda ws, team, win, wid: sidecar_calls.append((ws, team, win, wid)) or 1,
    )

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        name = str(kwargs["name"])
        team_name = str(kwargs["team_name"])
        cli_name = str(kwargs["cli"])
        cli_mod.tmux.tag_pane("%101", "agent", name, team_name, cli=cli_name)
        return Agent(name=name, team_name=team_name, pane_id="%101", cli=cli_name)

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))

    result = runner.invoke(cli, ["squad", "init", "--name", "peaky"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert breaks == ["%100"]                     # crowded origin → broke out
    assert payload["window"] == "dev:8"
    assert payload["team"] == "dev-w88"           # internal main team id-derived from final window
    assert payload["squadName"] == "peaky"         # squad-facing namespace stays readable
    assert payload["orch"]["name"] == "peaky.orch"
    assert payload["challenger"]["name"] == "peaky.challenger"
    assert spawned[0]["team_name"] == "dev-w88"   # challenger spawned under the final-window team
    assert payload["dispatched"] == ["peaky.challenger"]
    assert payload["next"] == "hive skills get squad-orch"
    orch_pane = payload["orch"]["pane"]
    assert not [c for c in sent if c[0] == orch_pane and "skills get" in c[1]]
    assert sidecar_calls == [("/tmp/hive-dev-w88", "dev-w88", "dev:8", "@88")]


def _bind_pane_as_squad_orch(cli_mod, pane="%100", *, team="dev-w0", squad="peaky"):
    cli_mod.tmux.tag_pane(pane, "agent", f"{squad}.orch", team, group=squad)


def test_squad_init_idempotent_on_bound_orch_with_same_name(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Re-running `hive squad init --name peaky` from an already-bound peaky.orch
    pane echoes the existing binding — no duplicate-name failure (the squad's own
    group must not count as a foreign claim), no retag, no second challenger."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _pane: SimpleNamespace(name="codex", skill_cmd="/{name}"))
    _bind_pane_as_squad_orch(cli_mod)

    spawned: list[dict] = []
    monkeypatch.setattr(
        cli_mod.Agent, "spawn",
        staticmethod(lambda **k: spawned.append(k) or Agent(name="x", team_name="t", pane_id="%999", cli="codex")),
    )
    broke: list[str] = []
    renamed: list = []
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: broke.append(p) or ("dev:9", "%900"))
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda *a: renamed.append(a))

    result = runner.invoke(cli, ["squad", "init", "--name", "peaky"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["team"] == "dev-w0"   # existing binding echoed, not a duplicate-name failure
    assert payload.get("group") == "peaky"
    assert spawned == []                  # no second challenger
    assert broke == [] and renamed == []  # no window mutation


def test_squad_init_idempotent_on_bound_orch_plain_rerun(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Plain `hive squad init` from a bound peaky.orch pane does not rename the
    squad namespace or spawn another challenger."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _pane: SimpleNamespace(name="codex", skill_cmd="/{name}"))
    _bind_pane_as_squad_orch(cli_mod)

    spawned: list[dict] = []
    monkeypatch.setattr(
        cli_mod.Agent, "spawn",
        staticmethod(lambda **k: spawned.append(k) or Agent(name="x", team_name="t", pane_id="%999", cli="codex")),
    )
    renamed: list = []
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda *a: renamed.append(a))

    result = runner.invoke(cli, ["squad", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["team"] == "dev-w0"
    assert payload.get("group") == "peaky"   # squad namespace unchanged
    assert spawned == []                       # no second challenger
    assert renamed == []                       # no window rename


# --- role config: challenger CLI + model via settings ---


def _squad_init_mocks(cli_mod, monkeypatch, repo):
    """Shared stubs for squad init role-config tests."""
    spawned: list[dict] = []
    profile = SimpleNamespace(name="codex", skill_cmd="/{name}")
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _pane: profile)
    monkeypatch.setattr(cli_mod, "family_for_pane", lambda _pane: "openai")
    monkeypatch.setattr(cli_mod, "resolve_peer_spawn", lambda **_kwargs: ("claude", ""))
    monkeypatch.setattr(cli_mod, "anti_peer_cli", lambda _c: "claude")
    monkeypatch.setattr(cli_mod.tmux, "get_current_window_index", lambda: "0")
    monkeypatch.setattr(cli_mod.tmux, "get_pane_count", lambda _pane: 1)
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda _t, _n: None)
    monkeypatch.setattr(
        cli_mod.tmux,
        "display_value",
        lambda _p, fmt: "dev:0" if fmt == "#{session_name}:#{window_index}" else str(repo),
    )
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda *_a, **_k: None)
    monkeypatch.setattr(cli_mod.tmux, "send_key", lambda *_a, **_k: None)
    monkeypatch.setattr(cli_mod.tmux, "select_window", lambda _t: None)
    monkeypatch.setattr(cli_mod.tmux, "configure_hive_window", lambda _t: None)
    monkeypatch.setattr("hive.team.tmux.configure_hive_window", lambda _t: None)
    monkeypatch.setattr("hive.sidecar.stop_sidecar", lambda _ws: None)
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _t, _c: True)
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda _t: SimpleNamespace(orientation="horizontal"))
    monkeypatch.setattr(cli_mod.time, "sleep", lambda _s: None)

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        name = str(kwargs["name"])
        team_name = str(kwargs["team_name"])
        cli_name = str(kwargs["cli"])
        cli_mod.tmux.tag_pane("%101", "agent", name, team_name, cli=cli_name)
        return Agent(name=name, team_name=team_name, pane_id="%101", cli=cli_name)

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))
    return spawned


def test_squad_init_uses_role_config_for_challenger(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned = _squad_init_mocks(cli_mod, monkeypatch, repo)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.challenger.cli": "droid",
            "roles.challenger.model": "opus",
        }.get(key, default),
    )

    result = runner.invoke(cli, ["squad", "init", "--name", "peaky"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "droid"
    assert spawned[0]["model"] == "opus"


def test_squad_init_flag_overrides_role_cli_keeps_model(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned = _squad_init_mocks(cli_mod, monkeypatch, repo)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.challenger.cli": "droid",
            "roles.challenger.model": "opus",
        }.get(key, default),
    )

    result = runner.invoke(cli, ["squad", "init", "--name", "peaky", "--peer-cli", "codex"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "codex"
    assert spawned[0]["model"] == "opus"


def test_squad_init_model_only_keeps_default_cli(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned = _squad_init_mocks(cli_mod, monkeypatch, repo)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.challenger.model": "o4-mini",
        }.get(key, default),
    )

    result = runner.invoke(cli, ["squad", "init", "--name", "peaky"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "claude"  # anti-family fallback
    assert spawned[0]["model"] == "o4-mini"
