import json
from types import SimpleNamespace

from hive.agent import Agent
from hive.cli import cli, _attach_duo_to_team as _real_attach_duo_to_team


def _duo_mocks(cli_mod, monkeypatch, repo, *, pane_count, family_map=None, panes=None):
    """Shared duo-init stubs: codex worker (openai family), claude validator.

    Returns the list of (pane, text) captured from cli-layer tmux.send_keys
    plus native Agent.send deliveries (adoption bootstrap goes over the
    native transport now) —
    role injection into the current pane would show up here."""
    sent: list = []
    # configure_hive_home stubs _attach_duo_to_team; restore the real one so
    # these tests exercise the actual duo-formation logic.
    monkeypatch.setattr(cli_mod, "_attach_duo_to_team", _real_attach_duo_to_team)
    profile = SimpleNamespace(name="codex", skill_cmd="/{name}")
    fam = family_map or {}
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _p: profile)
    monkeypatch.setattr(cli_mod, "family_for_pane", lambda p: fam.get(p, "openai"))
    monkeypatch.setattr(cli_mod, "peer_cli_for_family", lambda _f: "claude")
    monkeypatch.setattr(cli_mod, "anti_peer_cli", lambda _c: "claude")
    monkeypatch.setattr(cli_mod, "_require_codex_daemon_backed", lambda _p: None)
    monkeypatch.setattr(cli_mod, "_resolve_spawn_cli_name", lambda _a: "codex")
    monkeypatch.setattr(cli_mod, "_pane_is_idle_for_pairing", lambda _p: True)
    monkeypatch.setattr(cli_mod.tmux, "get_pane_count", lambda _p: pane_count)
    monkeypatch.setattr(cli_mod.tmux, "get_pane_window_target", lambda _p: "dev:0")
    # Placement reads one successful window snapshot (count + neighbors); tests
    # that don't pass explicit panes get one synthesized from pane_count.
    if panes is None:
        panes = [SimpleNamespace(pane_id="%100", team="", group="")] + [
            SimpleNamespace(pane_id=f"%15{i}", team="", group="") for i in range(pane_count - 1)
        ]
    monkeypatch.setattr(cli_mod.tmux, "list_panes_full_or_none", lambda _w: panes)
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
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda pane, text, **_k: sent.append((pane, text)))
    monkeypatch.setattr(cli_mod.tmux, "send_key", lambda *_a, **_k: None)
    monkeypatch.setattr(
        "hive.agent.Agent.send",
        lambda self, text: sent.append((self.pane_id, text)) or "mcpWriteAccepted",
    )
    monkeypatch.setattr("hive.sidecar.stop_sidecar", lambda _ws: None)
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _t, _c: True)
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda _t: SimpleNamespace(orientation="horizontal"))
    return sent


def test_duo_init_one_pane_spawns_antifamily_validator(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    sent = _duo_mocks(cli_mod, monkeypatch, repo, pane_count=1)

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id="%101",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["group"] == "duo"
    assert payload["worker"] == {"pane": "%100", "name": "worker", "cli": "codex"}
    assert payload["validator"]["name"] == "validator"
    assert payload["validator"]["mode"] == "spawned"
    assert payload["validator"]["pane"] == "%101"
    assert payload["validator"]["cli"] == "claude"  # anti-family of codex worker

    assert len(spawned) == 1
    assert spawned[0]["name"] == "validator"
    assert spawned[0]["cli"] == "claude"
    assert spawned[0]["skill"] == "none"
    # validator's role bootstrap is a thin pointer: the spawned pane loads its
    # spec itself via `hive skills get duo-validator` — the same CLI-served
    # channel a dispatched pane uses — so no spec snapshot is inlined into the
    # launch command or cached on disk.
    bootstrap = spawned[0]["prompt"]
    assert bootstrap == cli_mod._role_bootstrap_prompt("duo-validator")
    assert "hive skills get duo-validator" in bootstrap
    assert "别 `sleep` 轮询" in bootstrap
    assert "别退出" not in bootstrap
    assert payload["dispatched"] == ["validator"]
    assert payload["next"] == "hive skills get duo-worker"
    # The worker runs init itself: nothing may be injected into its pane.
    worker_pane = payload["worker"]["pane"]
    assert not [c for c in sent if c[0] == worker_pane and "skills get" in c[1]]
    # Spawned validator gets its role via the launch prompt, not injection.
    assert not [c for c in sent if "skills get duo-validator" in c[1]]


def test_role_bootstrap_prompts_do_not_tell_idle_agents_to_not_exit(configure_hive_home):
    from hive.cli import _role_bootstrap_prompt

    configure_hive_home()

    for role in ("duo-validator", "squad-challenger", "squad-worker", "squad-validator"):
        prompt = _role_bootstrap_prompt(role)
        assert "别退出" not in prompt
        assert "别 `sleep` 轮询" in prompt


def test_duo_init_two_panes_adopts_idle_antifamily_neighbor(
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
    sent = _duo_mocks(
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

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload["validator"]["mode"] == "paired"
    assert payload["validator"]["pane"] == "%150"
    assert payload["worker"]["pane"] == "%100"  # stays put, not broken out
    assert spawned == []  # adopted the neighbor, did not spawn
    assert breaks == []  # 2-pane pairable → no break-out
    assert payload["dispatched"] == ["validator"]  # adopted validator got its role injected
    assert payload["next"] == "hive skills get duo-worker"
    worker_pane = payload["worker"]["pane"]
    assert not [c for c in sent if c[0] == worker_pane and "skills get" in c[1]]
    # Positive control: the adopted idle neighbor gets the exact same
    # bootstrap prompt a spawned validator would get at launch.
    validator_pane = payload["validator"]["pane"]
    injected = [c[1] for c in sent if c[0] == validator_pane and "skills get" in c[1]]
    assert injected == [cli_mod._role_bootstrap_prompt("duo-validator")]


def test_duo_init_two_panes_same_family_breaks_out_then_spawns(
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
    sent = _duo_mocks(
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

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert breaks == ["%100"]  # unpairable → worker broken out
    assert payload["worker"]["pane"] == "%200"
    assert payload["window"] == "dev:1"
    assert payload["validator"]["mode"] == "spawned"
    assert len(spawned) == 1
    assert payload["dispatched"] == ["validator"]
    assert payload["next"] == "hive skills get duo-worker"
    # The worker runs init itself: nothing may be injected into its pane.
    worker_pane = payload["worker"]["pane"]
    assert not [c for c in sent if c[0] == worker_pane and "skills get" in c[1]]
    # Spawned validator gets its role via the launch prompt, not injection.
    assert not [c for c in sent if "skills get duo-validator" in c[1]]


def test_duo_init_three_panes_breaks_out_then_spawns(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    breaks: list[str] = []
    sent = _duo_mocks(cli_mod, monkeypatch, repo, pane_count=3)

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

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert breaks == ["%100"]  # crowded window → break the worker out
    assert payload["worker"]["pane"] == "%200"
    assert payload["window"] == "dev:1"
    assert payload["validator"]["mode"] == "spawned"
    assert len(spawned) == 1
    assert payload["dispatched"] == ["validator"]
    assert payload["next"] == "hive skills get duo-worker"
    # The worker runs init itself: nothing may be injected into its pane.
    worker_pane = payload["worker"]["pane"]
    assert not [c for c in sent if c[0] == worker_pane and "skills get" in c[1]]
    # Spawned validator gets its role via the launch prompt, not injection.
    assert not [c for c in sent if "skills get duo-validator" in c[1]]


def test_duo_init_breakout_names_team_from_final_window_not_origin(
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
    sent = _duo_mocks(cli_mod, monkeypatch, repo, pane_count=3)
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

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert breaks == ["%100"]
    assert payload["window"] == "dev:7"
    assert payload["team"] == "dev-w77"            # final window @77, not origin/index
    assert payload["worker"]["pane"] == "%200"
    assert spawned[0]["team_name"] == "dev-w77"    # validator spawned under the final-window team
    assert sidecar_calls == [("/tmp/hive-dev-w77", "dev-w77", "dev:7", "@77")]


def test_duo_window_name_branch_then_project(monkeypatch):
    """Duo window label = git branch with noise prefix stripped; falls back to
    the project basename on a default branch or outside git."""
    import hive.cli as cli_mod

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "feat/compose-creator-language")
    assert cli_mod._duo_window_name("/Users/x/ordo_ai") == "compose-creator-language"

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "worktree-kol-task-control-board")
    assert cli_mod._duo_window_name("/Users/x/ordo_ai") == "kol-task-control-board"

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "main")
    assert cli_mod._duo_window_name("/Users/notdp/Developer/hive") == "hive"

    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _c: "")
    assert cli_mod._duo_window_name("/Users/x/myproj") == "myproj"


def test_duo_init_window_named_after_git_branch(runner, configure_hive_home, monkeypatch, tmp_path):
    """A formed duo renames its window to the worker's feature branch (noise
    prefix stripped) instead of a generic 'duo'."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    sent = _duo_mocks(cli_mod, monkeypatch, repo, pane_count=1)
    monkeypatch.setattr(cli_mod, "_git_branch_for_cwd", lambda _cwd: "feat/compose-creator-language")
    renamed: list[tuple[str, str]] = []
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda target, name: renamed.append((target, name)))
    monkeypatch.setattr(
        cli_mod.Agent,
        "spawn",
        staticmethod(lambda **k: Agent(name="validator", team_name=str(k["team_name"]), pane_id="%101", cli="claude")),
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    # 1-pane window dev:0 renamed to the feature (feat/ prefix stripped), not "duo".
    assert ("dev:0", "compose-creator-language") in renamed


def test_unique_duo_window_name_appends_counter_on_collision(monkeypatch):
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
    assert cli_mod._unique_duo_window_name("kol-task-control-board", "dev:2") == "kol-task-control-board"
    # base + -2 both taken → -3
    assert cli_mod._unique_duo_window_name("compose-creator-language", "dev:2") == "compose-creator-language-3"
    # this_window's own name is excluded → no self-collision
    assert cli_mod._unique_duo_window_name("other", "dev:9") == "other"


def test_inject_role_bootstrap_sends_full_prompt_once(monkeypatch):
    """Adoption delivers the same bootstrap prompt as a spawn launch — exact
    text from _role_bootstrap_prompt, over the pane's native transport (no
    keystrokes)."""
    from types import SimpleNamespace

    import hive.cli as cli_mod

    sent: list = []
    keys: list = []
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _p: SimpleNamespace(name="claude"))
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda pane, text, **_k: keys.append((pane, text)))
    monkeypatch.setattr(cli_mod.tmux, "send_key", lambda pane, key: keys.append((pane, key)))
    monkeypatch.setattr(
        "hive.agent.Agent.send", lambda self, text: sent.append((self.pane_id, text)) or "mcpWriteAccepted"
    )
    assert cli_mod._inject_role_bootstrap("%9", "duo-validator") is True
    assert sent == [("%9", cli_mod._role_bootstrap_prompt("duo-validator"))]
    assert keys == []


def test_inject_role_bootstrap_fails_when_transport_refuses(monkeypatch):
    """A refused native transport returns False — never a keystroke fallback."""
    from types import SimpleNamespace

    import hive.cli as cli_mod
    from hive.agent import DeliveryError

    keys: list = []
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _p: SimpleNamespace(name="claude"))
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda pane, text, **_k: keys.append((pane, text)))

    def _refuse(self, text):
        raise DeliveryError("no channel")

    monkeypatch.setattr("hive.agent.Agent.send", _refuse)
    assert cli_mod._inject_role_bootstrap("%9", "duo-validator") is False
    assert keys == []


def test_inject_role_bootstrap_noop_without_agent_profile(monkeypatch):
    import hive.cli as cli_mod

    sent: list = []
    monkeypatch.setattr(cli_mod, "detect_profile_for_pane", lambda _p: None)
    monkeypatch.setattr(cli_mod.tmux, "send_keys", lambda *a, **_k: sent.append(a))
    monkeypatch.setattr(cli_mod.tmux, "send_key", lambda *a, **_k: sent.append(a))
    assert cli_mod._inject_role_bootstrap("%9", "duo-validator") is False
    assert sent == []


def test_role_bootstrap_prompt_is_single_line():
    """tmux input boxes submit one line; a newline would split the prompt."""
    import hive.cli as cli_mod

    for role in ("duo-worker", "duo-validator", "squad-orch", "squad-challenger", "squad-worker", "squad-validator"):
        assert "\n" not in cli_mod._role_bootstrap_prompt(role)


# --- role config: validator CLI + model via settings ---


def test_duo_init_uses_role_config_for_validator(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """When roles.validator.cli/model are set, duo init uses them for the spawned validator."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    _duo_mocks(cli_mod, monkeypatch, repo, pane_count=1)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
            "roles.validator.model": "o3",
        }.get(key, default),
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

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "codex"
    assert spawned[0]["model"] == "o3"


def test_duo_init_flag_overrides_role_cli_but_keeps_model(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """--validator-cli flag overrides roles.validator.cli but roles.validator.model still applies."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    _duo_mocks(cli_mod, monkeypatch, repo, pane_count=1)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
            "roles.validator.model": "o3",
        }.get(key, default),
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

    result = runner.invoke(cli, ["duo", "init", "--validator-cli", "claude"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "claude"
    assert spawned[0]["model"] == "o3"


def test_duo_init_model_only_keeps_default_cli(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Model-only config: CLI falls back to anti-family, model from roles config."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    _duo_mocks(cli_mod, monkeypatch, repo, pane_count=1)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.model": "opus",
        }.get(key, default),
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

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "claude"  # anti-family fallback
    assert spawned[0]["model"] == "opus"


def test_duo_init_role_cli_set_without_model_no_stale_peer_model(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """When roles.validator.cli is set but model is not, no stale peer model leaks."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    _duo_mocks(cli_mod, monkeypatch, repo, pane_count=1)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
        }.get(key, default),
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

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    assert spawned[0]["cli"] == "codex"
    assert spawned[0]["model"] == ""


# --- init-time revive of a dead validator (idempotent path) ---


def _revive_binding(ws: str) -> dict:
    return {
        "team": "dev-w2",
        "workspace": ws,
        "agent": "worker",
        "role": "agent",
        "pane": "%100",
        "tmuxSession": "dev",
        "tmuxWindow": "dev:0",
        "group": "duo",
    }


def _worker_pane_info():
    from hive.tmux import PaneInfo

    return PaneInfo(pane_id="%100", title="[worker]", team="dev-w2", agent="worker", group="duo")


def _validator_pane_info():
    from hive.tmux import PaneInfo

    return PaneInfo(pane_id="%101", title="[validator]", team="dev-w2", agent="validator", group="duo")


def _revive_mocks(cli_mod, monkeypatch, tmp_path, *, panes, binding, team="default"):
    """Stubs for the idempotent-init revive path. Returns (spawned, peered, layouts)."""
    spawned: list[dict] = []
    peered: list[tuple] = []
    layouts: list[str] = []
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%100")
    monkeypatch.setattr(
        cli_mod, "detect_profile_for_pane", lambda _p: SimpleNamespace(name="claude", skill_cmd="/{name}")
    )
    monkeypatch.setattr(cli_mod, "_require_codex_daemon_backed", lambda _p: None)
    monkeypatch.setattr(cli_mod, "_discover_tmux_binding", lambda: binding)
    monkeypatch.setattr(cli_mod.tmux, "list_panes_full_or_none", lambda _w: panes)
    monkeypatch.setattr(cli_mod, "family_for_pane", lambda _p: "anthropic")
    monkeypatch.setattr(cli_mod, "peer_cli_for_family", lambda _f: "codex")
    monkeypatch.setattr(cli_mod.tmux, "display_value", lambda _p, _fmt: str(tmp_path / "repo"))
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.hive_context, "save_context_for_pane", lambda *_a, **_k: None)
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _t, _c: True)
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda t: layouts.append(t))
    if team == "default":
        team = SimpleNamespace(
            name=binding["team"],
            tmux_window=binding["tmuxWindow"],
            workspace=binding["workspace"],
            agents={"worker": SimpleNamespace(pane_id=binding["pane"])},
            set_peer=lambda a, b: peered.append((a, b)),
        )
    if isinstance(team, Exception):
        def _load(_name, prefer_pane=""):
            raise team
        monkeypatch.setattr(cli_mod.Team, "load", staticmethod(_load))
    else:
        monkeypatch.setattr(cli_mod.Team, "load", staticmethod(lambda _n, prefer_pane="": team))

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id="%777",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))
    return spawned, peered, layouts


def test_duo_init_revives_missing_validator(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    ws = str(tmp_path / "ws")
    spawned, peered, layouts = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[_worker_pane_info()], binding=_revive_binding(ws)
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)

    assert len(spawned) == 1
    assert spawned[0]["name"] == "validator"
    assert spawned[0]["cli"] == "codex"  # anti-family of the claude worker
    assert spawned[0]["skill"] == "none"
    assert spawned[0]["prompt"] == cli_mod._role_bootstrap_prompt("duo-validator")
    assert spawned[0]["workspace"] == ws
    assert spawned[0]["cwd"] == str(tmp_path / "repo")
    assert peered == [("worker", "validator")]
    assert layouts == ["dev:0"]
    assert payload["team"] == "dev-w2"
    assert payload["validator"] == {
        "pane": "%777",
        "name": "validator",
        "cli": "codex",
        "mode": "respawned",
    }


def test_hive_init_revives_missing_validator(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    ws = str(tmp_path / "ws")
    spawned, _, _ = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[_worker_pane_info()], binding=_revive_binding(ws)
    )

    result = runner.invoke(cli, ["init"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)

    assert len(spawned) == 1
    assert payload["validator"]["mode"] == "respawned"


def test_duo_init_rerun_with_live_validator_spawns_nothing(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    ws = str(tmp_path / "ws")
    binding = _revive_binding(ws)
    spawned, _, _ = _revive_mocks(
        cli_mod,
        monkeypatch,
        tmp_path,
        panes=[_worker_pane_info(), _validator_pane_info()],
        binding=binding,
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)

    assert spawned == []
    assert payload == binding  # existing binding shape, untouched


def test_duo_init_does_not_revive_on_tmux_listing_failure(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """A tmux failure is unknown, not 'validator missing' — never spawn on it."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    ws = str(tmp_path / "ws")
    binding = _revive_binding(ws)
    spawned, _, _ = _revive_mocks(cli_mod, monkeypatch, tmp_path, panes=None, binding=binding)

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)

    assert spawned == []
    assert "validator" not in payload


def test_duo_init_does_not_revive_when_worker_absent_from_listing(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    spawned, _, _ = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[], binding=_revive_binding(str(tmp_path / "ws"))
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    assert spawned == []


def test_duo_init_does_not_revive_non_duo_or_non_worker(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    for patch in ({"group": "squad"}, {"agent": "validator"}):
        binding = {**_revive_binding(str(tmp_path / "ws")), **patch}
        spawned, _, _ = _revive_mocks(
            cli_mod, monkeypatch, tmp_path, panes=[_worker_pane_info()], binding=binding
        )
        result = runner.invoke(cli, ["duo", "init"])
        assert result.exit_code == 0, result.output
        assert spawned == []


def test_duo_init_does_not_revive_when_team_load_fails(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    spawned, _, _ = _revive_mocks(
        cli_mod,
        monkeypatch,
        tmp_path,
        panes=[_worker_pane_info()],
        binding=_revive_binding(str(tmp_path / "ws")),
        team=ValueError("gone"),
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output
    assert spawned == []


def test_duo_init_does_not_revive_when_worker_not_in_team(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    lonely = SimpleNamespace(name="dev-w2", agents={}, set_peer=lambda a, b: None)
    spawned, _, _ = _revive_mocks(
        cli_mod,
        monkeypatch,
        tmp_path,
        panes=[_worker_pane_info()],
        binding=_revive_binding(str(tmp_path / "ws")),
        team=lonely,
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output
    assert spawned == []


def test_revive_uses_role_config_for_validator(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    spawned, _, _ = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[_worker_pane_info()], binding=_revive_binding(str(tmp_path / "ws"))
    )
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
            "roles.validator.model": "o3",
        }.get(key, default),
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "codex"
    assert spawned[0]["model"] == "o3"


def test_revive_flag_overrides_role_cli_but_keeps_model(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    spawned, _, _ = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[_worker_pane_info()], binding=_revive_binding(str(tmp_path / "ws"))
    )
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
            "roles.validator.model": "o3",
        }.get(key, default),
    )

    result = runner.invoke(cli, ["duo", "init", "--validator-cli", "claude"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["cli"] == "claude"
    assert spawned[0]["model"] == "o3"


def test_duo_init_does_not_revive_when_listed_pane_identity_mismatches(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Same pane id in the listing but wrong team/agent/group — stale binding, zero spawn."""
    from hive.tmux import PaneInfo

    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    intruder = PaneInfo(pane_id="%100", title="", team="other", agent="intruder", group="squad")
    spawned, _, _ = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[intruder], binding=_revive_binding(str(tmp_path / "ws"))
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)

    assert spawned == []
    assert "validator" not in payload


def test_duo_init_does_not_revive_when_loaded_team_mismatches_binding(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Loaded team pointing at another window or worker pane — zero spawn."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    wrong_window = SimpleNamespace(
        name="dev-w2",
        tmux_window="dev:9",
        agents={"worker": SimpleNamespace(pane_id="%100")},
        set_peer=lambda a, b: None,
    )
    wrong_worker_pane = SimpleNamespace(
        name="dev-w2",
        tmux_window="dev:0",
        agents={"worker": SimpleNamespace(pane_id="%999")},
        set_peer=lambda a, b: None,
    )
    for stale in (wrong_window, wrong_worker_pane):
        spawned, _, _ = _revive_mocks(
            cli_mod,
            monkeypatch,
            tmp_path,
            panes=[_worker_pane_info()],
            binding=_revive_binding(str(tmp_path / "ws")),
            team=stale,
        )
        result = runner.invoke(cli, ["duo", "init"])
        assert result.exit_code == 0, result.output
        assert spawned == []
        assert "validator" not in json.loads(result.output)


def test_duo_init_aborts_before_mutation_on_listing_failure(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """An unanswered pane listing must abort fresh placement before any
    break_pane / spawn — unknown is not a 1-pane window."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    breaks: list[str] = []
    _duo_mocks(cli_mod, monkeypatch, repo, pane_count=2)
    monkeypatch.setattr(cli_mod.tmux, "list_panes_full_or_none", lambda _w: None)
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: breaks.append(p) or ("dev:1", "%200"))
    monkeypatch.setattr(
        cli_mod.Agent, "spawn", staticmethod(lambda **k: spawned.append(k))
    )

    result = runner.invoke(cli, ["duo", "init"])

    assert result.exit_code != 0
    assert breaks == []
    assert spawned == []


def test_duo_init_aborts_when_current_pane_missing_from_listing(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    repo = tmp_path / "repo"
    repo.mkdir()
    spawned: list[dict] = []
    breaks: list[str] = []
    _duo_mocks(
        cli_mod,
        monkeypatch,
        repo,
        pane_count=2,
        panes=[SimpleNamespace(pane_id="%777", team="", group="")],
    )
    monkeypatch.setattr(cli_mod.tmux, "break_pane", lambda p, **k: breaks.append(p) or ("dev:1", "%200"))
    monkeypatch.setattr(
        cli_mod.Agent, "spawn", staticmethod(lambda **k: spawned.append(k))
    )

    result = runner.invoke(cli, ["duo", "init"])

    assert result.exit_code != 0
    assert breaks == []
    assert spawned == []


def test_revive_uses_verified_team_workspace_over_binding(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """The binding's workspace read can fold a tmux failure into ''; spawn and
    context must use the identity-verified team's workspace instead."""
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    binding = _revive_binding("")
    team = SimpleNamespace(
        name="dev-w2",
        tmux_window="dev:0",
        workspace="/real/ws",
        agents={"worker": SimpleNamespace(pane_id="%100")},
        set_peer=lambda a, b: None,
    )
    spawned, _, _ = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[_worker_pane_info()], binding=binding, team=team
    )
    contexts: list[tuple] = []
    monkeypatch.setattr(
        cli_mod.hive_context,
        "save_context_for_pane",
        lambda pane, **kw: contexts.append((pane, kw)),
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    assert len(spawned) == 1
    assert spawned[0]["workspace"] == "/real/ws"
    assert contexts and contexts[0][1]["workspace"] == "/real/ws"


def test_revive_zero_spawn_when_team_workspace_unknown(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    binding = _revive_binding("")
    team = SimpleNamespace(
        name="dev-w2",
        tmux_window="dev:0",
        workspace="",
        agents={"worker": SimpleNamespace(pane_id="%100")},
        set_peer=lambda a, b: None,
    )
    spawned, _, _ = _revive_mocks(
        cli_mod, monkeypatch, tmp_path, panes=[_worker_pane_info()], binding=binding, team=team
    )

    result = runner.invoke(cli, ["duo", "init"])
    assert result.exit_code == 0, result.output

    assert spawned == []
    assert "validator" not in json.loads(result.output)
