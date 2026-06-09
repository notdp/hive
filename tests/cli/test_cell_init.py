import json
from pathlib import Path
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
    # validator's role bootstrap (preamble + spec) is fed via a cached file
    # referenced by `"$(cat <path>)"`, not inlined into the launch command — so
    # the spawned no-human pane still operates on the full bootstrap directly
    # (no run-and-stop) while the launch stays short.
    assert not spawned[0].get("prompt")
    bootstrap_path = Path(spawned[0]["prompt_file"])
    assert bootstrap_path.name == "role-bootstrap-cell-validator.txt"
    bootstrap = bootstrap_path.read_text()
    assert "cell-validator" in bootstrap
    assert "等你的第一条任务消息" in bootstrap
    assert payload["dispatched"] == ["worker", "validator"]


def test_role_bootstrap_file_caches_byte_exact_and_rewrites_on_drift(
    configure_hive_home, tmp_path
):
    from hive.cli import _role_bootstrap_file, _role_bootstrap_prompt

    configure_hive_home()

    path = _role_bootstrap_file("cell-validator")
    assert path.name == "role-bootstrap-cell-validator.txt"
    # Byte-exact (no trailing newline): the `$(cat <file>)` replay must equal the
    # old inline prompt, or "zero behavior change" wouldn't hold.
    assert path.read_text() == _role_bootstrap_prompt("cell-validator")

    # Idempotent: same path, content preserved.
    assert _role_bootstrap_file("cell-validator") == path

    # Drift: stale cached content is rewritten to match current code.
    path.write_text("stale")
    assert _role_bootstrap_file("cell-validator").read_text() == _role_bootstrap_prompt(
        "cell-validator"
    )


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


def test_cell_init_breakout_names_team_from_final_window_not_origin(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Bug A: after break-out the team name + workspace + validator team follow
    the FINAL window's stable id, not the origin window (dev:0) or its index."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    breaks: list[str] = []
    # 3-pane origin (dev:0) → worker breaks out to a fresh window dev:7 whose id
    # slug (@77) differs from its index (7), proving the name is id-derived.
    _cell_mocks(cli_mod, monkeypatch, repo, pane_count=3)
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: breaks.append(p) or ("dev:7", "%200"))
    monkeypatch.setattr(cli_mod.tmux, "get_window_id", lambda target: "@77" if target == "dev:7" else "@0")

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id="%101",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))

    sidecar_calls: list[tuple[str, str, str, str]] = []
    monkeypatch.setattr(
        "hive.sidecar.ensure_sidecar",
        lambda ws, team, win, wid: sidecar_calls.append((ws, team, win, wid)) or 1,
    )

    result = runner.invoke(cli, ["cell", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert breaks == ["%100"]
    assert payload["window"] == "dev:7"
    assert payload["team"] == "dev-w77"            # final window @77, not origin/index
    assert payload["worker"]["pane"] == "%200"
    assert spawned[0]["team_name"] == "dev-w77"    # validator spawned under the final-window team
    assert sidecar_calls == [("/tmp/hive-dev-w77", "dev-w77", "dev:7", "@77")]


def test_cell_window_name_branch_then_project(monkeypatch):
    """Cell window label = git branch with noise prefix stripped; falls back to
    the project basename on a default branch or outside git."""
    import hive.cli as cli_mod

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "feat/compose-creator-language")
    assert cli_mod._cell_window_name("/Users/x/ordo_ai") == "compose-creator-language"

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "worktree-kol-task-control-board")
    assert cli_mod._cell_window_name("/Users/x/ordo_ai") == "kol-task-control-board"

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "main")
    assert cli_mod._cell_window_name("/Users/notdp/Developer/hive") == "hive"

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "")
    assert cli_mod._cell_window_name("/Users/x/myproj") == "myproj"


def test_cell_init_window_named_after_git_branch(runner, configure_hive_home, monkeypatch, tmp_path):
    """A formed cell renames its window to the worker's feature branch (noise
    prefix stripped) instead of a generic 'cell'."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    _cell_mocks(cli_mod, monkeypatch, repo, pane_count=1)
    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _cwd: "feat/compose-creator-language")
    renamed: list[tuple[str, str]] = []
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda target, name: renamed.append((target, name)))
    monkeypatch.setattr(
        cli_mod.Agent,
        "spawn",
        staticmethod(lambda **k: Agent(name="validator", team_name=str(k["team_name"]), pane_id="%101", cli="claude")),
    )

    result = runner.invoke(cli, ["cell", "init"])
    assert result.exit_code == 0, result.output

    # 1-pane window dev:0 renamed to the feature (feat/ prefix stripped), not "cell".
    assert ("dev:0", "compose-creator-language") in renamed


def test_unique_cell_window_name_appends_counter_on_collision(monkeypatch):
    """Same-branch siblings get -2, -3 ...; a free name stays clean; a window
    never collides with its own name."""
    import hive.cli as cli_mod

    monkeypatch.setattr(
        cli_mod.tmux,
        "list_window_names",
        lambda: [
            ("dev:1", "compose-creator-language"),
            ("dev:5", "compose-creator-language-2"),
            ("dev:9", "other"),
        ],
    )
    # free name → unchanged
    assert cli_mod._unique_cell_window_name("kol-task-control-board", "dev:2") == "kol-task-control-board"
    # base + -2 both taken → -3
    assert cli_mod._unique_cell_window_name("compose-creator-language", "dev:2") == "compose-creator-language-3"
    # this_window's own name is excluded → no self-collision
    assert cli_mod._unique_cell_window_name("other", "dev:9") == "other"
