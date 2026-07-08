"""Tests for `hive squad spawn-duo`: readiness polling + G7 auto-placement.

`squad_spawn_duo_cmd` blocks until both freshly-spawned peer panes report
`inputState=ready` via sidecar team-runtime, otherwise it fails with
`spawn_ready_timeout` JSON and a non-zero exit code. `inputState=ready`
corresponds to the sidecar's input-gate reading the transcript tail as
"clear" — which only happens after the dispatched skill's bootstrap turn
finishes (the `hive team` self-identification call returns and the CLI
settles into idle). The poll itself lives in `_wait_for_peer_ready`.

(`turnPhase` is intentionally NOT part of the readiness gate: it derives
from a separate transcript-tail probe that does not converge reliably for
freshly-spawned peer panes, and its value is orthogonal to "skill loaded".)

G7 adds: CLI takes no positional arg, and the peer is placed at an explicit
tmux window index >= 1000 (monotonic) so it never collides with the user's
regular low-index windows. The index is computed by `_next_squad_window_index`
and fed to `tmux.new_window(..., index=n)`; window name defaults to
`pending` (orch renames as the lifecycle advances).
"""

import pytest

from hive.cli import (
    _SQUAD_PEER_WINDOW_NAME_INITIAL,
    _next_peer_index_in_range,
    _wait_for_peer_ready,
    cli,
)


def _member(input_state: str, turn_phase: str) -> dict[str, str]:
    return {"inputState": input_state, "turnPhase": turn_phase}


def test_wait_for_peer_ready_returns_immediately_when_all_ready(monkeypatch):
    """(a) all-ready path: first team-runtime poll shows both agents ready.

    The helper must not sleep and must call the sidecar exactly once.
    """

    calls: list[tuple[str, str]] = []

    def fake_runtime(workspace: str, *, team: str):
        calls.append((workspace, team))
        return {
            "members": {
                "squad.worker-1": _member("ready", "turn_closed"),
                "squad.validator-1": _member("ready", "turn_closed"),
            }
        }

    sleeps: list[float] = []
    monkeypatch.setattr("hive.sidecar.request_team_runtime", fake_runtime)
    monkeypatch.setattr("hive.cli.time.sleep", lambda seconds: sleeps.append(seconds))

    not_ready = _wait_for_peer_ready(
        "/tmp/ws",
        team_name="t-duo-1",
        agents={"squad.worker-1", "squad.validator-1"},
    )

    assert not_ready == set()
    assert calls == [("/tmp/ws", "t-duo-1")]
    assert sleeps == []  # no retry needed


def test_wait_for_peer_ready_polls_until_all_eventually_ready(monkeypatch):
    """(b) eventually-ready path: worker lags behind validator.

    Poll 1: worker's CLI is up but skill hasn't finished the bootstrap turn
    (inputState=busy); validator is already ready. Poll 2: worker's input
    gate finally clears. `turnPhase` is incidental — we only sleep so long
    as one of the agents has NOT reached inputState=ready.
    """

    responses = iter([
        # Poll 1: worker still running its bootstrap turn.
        {
            "members": {
                "squad.worker-1": _member("busy", "tool_open"),
                "squad.validator-1": _member("ready", "turn_closed"),
            }
        },
        # Poll 2: worker's bootstrap turn has closed; input gate is clear.
        # turnPhase is deliberately still "tool_open" to assert the readiness
        # gate ignores it (skill is loaded as soon as inputState is ready).
        {
            "members": {
                "squad.worker-1": _member("ready", "tool_open"),
                "squad.validator-1": _member("ready", "turn_closed"),
            }
        },
    ])
    sleeps: list[float] = []

    monkeypatch.setattr(
        "hive.sidecar.request_team_runtime",
        lambda workspace, *, team: next(responses),
    )
    monkeypatch.setattr("hive.cli.time.sleep", lambda seconds: sleeps.append(seconds))

    not_ready = _wait_for_peer_ready(
        "/tmp/ws",
        team_name="t-duo-1",
        agents={"squad.worker-1", "squad.validator-1"},
    )

    assert not_ready == set()
    # One sleep: after poll 1; no sleep after poll 2 (all ready, loop exits).
    assert sleeps == [0.5]


def test_wait_for_peer_ready_times_out_and_returns_not_ready(monkeypatch):
    """(c) timeout path: sidecar never reports ready within 30s.

    Helper must return the still-waiting set; caller is responsible for
    emitting `spawn_ready_timeout` JSON + non-zero exit. We mock
    `time.monotonic` to leap past the deadline after the first iteration
    so the test doesn't actually wait 30 seconds.
    """

    # monotonic called twice per iteration: once at deadline init, once in
    # the while-guard. Feed first call 0.0, then 999.9 to immediately trip
    # the deadline on the second call.
    clock = iter([0.0, 999.9, 999.9, 999.9])
    monkeypatch.setattr("hive.cli.time.monotonic", lambda: next(clock))
    monkeypatch.setattr("hive.cli.time.sleep", lambda _seconds: None)
    monkeypatch.setattr(
        "hive.sidecar.request_team_runtime",
        lambda workspace, *, team: {
            "members": {
                "squad.worker-1": _member("busy", "tool_open"),
                "squad.validator-1": _member("busy", "tool_open"),
            }
        },
    )

    not_ready = _wait_for_peer_ready(
        "/tmp/ws",
        team_name="t-duo-1",
        agents={"squad.worker-1", "squad.validator-1"},
    )

    assert not_ready == {"squad.worker-1", "squad.validator-1"}


# --- squad range: tmux window index allocation per squad ---
#
# Each squad owns a 1000-wide slice of peer indices (peaky 1000-1999, krays
# 2000-2999, ...). `_next_peer_index_in_range(session, base)` picks the
# next unused index strictly inside [base, base+999]. Retired slots are
# NOT refilled — peer indices are stable for their lifetime.


def test_peer_index_starts_at_range_base_when_empty(monkeypatch):
    """No peer windows in the squad's range yet → start at base.

    peaky base=1000; session has user windows 1,2,7 + another squad's peer
    at 2500 (out of peaky's range). First peaky peer lands at 1000.
    """
    monkeypatch.setattr("hive.tmux.list_window_indices", lambda session: [1, 2, 7, 2500])
    assert _next_peer_index_in_range("613", 1000) == 1000


def test_peer_index_monotonic_within_range(monkeypatch):
    """Peer at base / base+1 already → next is base+2 (monotonic)."""
    monkeypatch.setattr(
        "hive.tmux.list_window_indices", lambda session: [1, 2, 1000, 1001]
    )
    assert _next_peer_index_in_range("613", 1000) == 1002


def test_peer_index_skips_gaps(monkeypatch):
    """Retired slot 1002 stays empty; next is 1004 (strict monotonic)."""
    monkeypatch.setattr(
        "hive.tmux.list_window_indices", lambda session: [1000, 1001, 1003]
    )
    assert _next_peer_index_in_range("613", 1000) == 1004


def test_peer_index_isolates_ranges(monkeypatch):
    """krays peers (2000-2999) don't touch peaky's counter (1000-1999).

    Even if krays has peers up to 2500, peaky with no peers still starts
    its first peer at 1000 — the range scheme guarantees per-squad slots.
    """
    monkeypatch.setattr(
        "hive.tmux.list_window_indices",
        lambda session: [1, 2, 2000, 2001, 2500],
    )
    assert _next_peer_index_in_range("613", 1000) == 1000
    assert _next_peer_index_in_range("613", 2000) == 2501


def test_peer_index_fails_when_range_exhausted(runner, monkeypatch):
    """Range [1000, 1999] fully used → _fail is called (SystemExit)."""
    monkeypatch.setattr(
        "hive.tmux.list_window_indices",
        lambda session: list(range(1000, 2000)),
    )
    with pytest.raises(SystemExit):
        _next_peer_index_in_range("613", 1000)


def test_spawn_peer_default_window_name_is_pending():
    """Initial window name is `pending` — caller prefixes with squad name.

    Spawn-peer uses `<squad>-pending` as placeholder before the atomic
    dispatch renames to `<squad>-<feature>-running`.
    """
    assert _SQUAD_PEER_WINDOW_NAME_INITIAL == "pending"


def test_squad_spawn_duo_cmd_rejects_positional_arg(runner):
    """CLI doesn't accept a positional N. Old invocation (`spawn-duo 1`) must
    fail before reaching any runtime code. Now that `--feature-id` + `--task`
    are required, the first failure click surfaces is a missing-option error,
    which equally proves the positional wasn't consumed.
    """
    result = runner.invoke(cli, ["squad", "spawn-duo", "1"])
    assert result.exit_code != 0
    out = result.output.lower()
    assert (
        "missing option" in out      # new required options short-circuit first
        or "unexpected" in out       # legacy: extra positional error
        or "got" in out
    )


def test_squad_spawn_duo_cmd_requires_feature_id_and_task(runner):
    """Bare `hive squad spawn-duo` (no flags) must fail fast — the atomic
    dispatch contract requires feature-id + task artifact so the peer never
    boots into an empty inbox.
    """
    result = runner.invoke(cli, ["squad", "spawn-duo"])
    assert result.exit_code != 0
    assert "missing option" in result.output.lower()
    assert "--feature-id" in result.output.lower() or "--task" in result.output.lower()


def _explode(name):
    def _boom(*_args, **_kwargs):
        raise AssertionError(f"{name} must not run for an invalid feature id")

    return _boom


def test_spawn_duo_rejects_step_style_feature_id_before_any_side_effect(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """`F2-03_04`-style ids (the live-run window-name mess) die in validation —
    before any window creation, option write, rename, spawn, or dispatch."""
    configure_hive_home()
    for fn in ("new_window", "set_window_option", "rename_window"):
        monkeypatch.setattr(f"hive.cli.tmux.{fn}", _explode(fn))
    task = tmp_path / "task.md"
    task.write_text("# task")

    result = runner.invoke(
        cli, ["squad", "spawn-duo", "--feature-id", "F2-03_04", "--task", str(task)]
    )

    assert result.exit_code == 1
    assert "kebab-case" in result.output
    assert "features.json" in result.output


def test_spawn_duo_help_teaches_semantic_feature_ids(runner):
    """The old help literally taught the spec's anti-example (`e.g. F1`)."""
    result = runner.invoke(cli, ["squad", "spawn-duo", "--help"])
    assert "e.g. F1" not in result.output
    assert "kebab-case" in result.output


# --- role config: worker + validator CLI/model in squad spawn-duo ---


def test_resolve_squad_worker_config_role_cli_in_precedence(monkeypatch):
    """roles.worker.cli sits between @hive-squad-worker tag and legacy squad.duoWorker."""
    import hive.cli as cli_mod

    monkeypatch.setattr(cli_mod.tmux, "get_window_option", lambda _w, _k: "")
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.worker.cli": "codex",
            "roles.worker.model": "opus",
            "squad.duoWorker": "claude",
        }.get(key, default),
    )
    monkeypatch.setattr(cli_mod.tmux, "get_pane_option", lambda _p, k: "claude" if k == "hive-cli" else "")

    cli_name, model = cli_mod._resolve_squad_worker_config("%1", "dev:1")
    assert cli_name == "codex"
    assert model == "opus"


def test_resolve_squad_worker_config_validator_role_config(monkeypatch):
    """resolve_role_config('validator') provides cli+model for squad spawn-duo validator."""
    from hive.settings import resolve_role_config

    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
            "roles.validator.model": "o3",
        }.get(key, default),
    )

    cli_name, model = resolve_role_config("validator")
    assert cli_name == "codex"
    assert model == "o3"


def test_resolve_squad_worker_config_validator_fallback_to_anti_peer(monkeypatch):
    """When roles.validator.cli is unset, validator falls back to anti_peer_cli."""
    from hive.agent_cli import anti_peer_cli
    from hive.settings import resolve_role_config

    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.model": "o3",
        }.get(key, default),
    )

    cli_name, model = resolve_role_config("validator")
    assert cli_name == ""  # not set → caller uses anti_peer_cli fallback
    assert model == "o3"
    assert anti_peer_cli("claude") == "codex"  # confirms fallback behavior


# --- full CLI-path spawn kwargs test for squad spawn-duo ---


def test_squad_spawn_duo_passes_role_config_to_agent_spawn(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """End-to-end: `hive squad spawn-duo` resolves role config and passes
    cli + model to both worker and validator Agent.spawn calls."""
    configure_hive_home(current_pane="%100", session_name="dev")

    import hive.cli as cli_mod
    from hive.agent import Agent
    from hive.team import Team
    from types import SimpleNamespace

    # Role config: worker=claude/opus, validator=codex/o3
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.worker.cli": "claude",
            "roles.worker.model": "opus",
            "roles.validator.cli": "codex",
            "roles.validator.model": "o3",
        }.get(key, default),
    )

    # Orch pane is tagged as peaky.orch in a squad window
    monkeypatch.setattr(
        cli_mod.tmux, "get_pane_option",
        lambda pane, key: {
            "hive-group": "peaky",
            "hive-cli": "claude",
        }.get(key, ""),
    )
    monkeypatch.setattr(
        cli_mod.tmux, "get_window_option",
        lambda win, key: {
            "hive-squad-base": "1000",
            "hive-squad-name": "peaky",
        }.get(key, ""),
    )

    # Fake team for _resolve_scoped_team
    fake_team = Team(
        name="dev-w0",
        workspace=str(tmp_path / "ws"),
        tmux_session="dev",
        tmux_window="dev:0",
        tmux_window_id="@0",
    )
    (tmp_path / "ws").mkdir()
    monkeypatch.setattr(
        cli_mod, "_resolve_scoped_team",
        lambda _team, required=True: ("dev-w0", fake_team),
    )

    # Tmux stubs
    monkeypatch.setattr(cli_mod.tmux, "list_window_indices", lambda _s: [0])
    monkeypatch.setattr(cli_mod.tmux, "list_panes_all", lambda: [])
    monkeypatch.setattr(cli_mod.tmux, "list_panes", lambda _w: ["%200"])
    monkeypatch.setattr(
        cli_mod.tmux, "new_window",
        lambda _s, name="", cwd="", index=0: (f"dev:{index}", "%200"),
    )
    monkeypatch.setattr(cli_mod.tmux, "set_window_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "configure_hive_window", lambda _t: None)
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda _t, _n: None)
    monkeypatch.setattr(cli_mod.tmux, "get_window_id", lambda _t: "@99")
    monkeypatch.setattr(
        cli_mod.tmux, "display_value",
        lambda _p, _f: str(tmp_path),
    )
    monkeypatch.setattr(
        "hive.layout.split_horizontal", lambda _t, _c: True,
    )
    monkeypatch.setattr(
        "hive.layout.apply_adaptive",
        lambda _t: SimpleNamespace(orientation="horizontal"),
    )

    # Capture Agent.spawn kwargs
    spawned: list[dict] = []

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id=f"%{200 + len(spawned)}",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))

    # Sidecar + readiness: instant ready
    monkeypatch.setattr(
        cli_mod, "_ensure_team_sidecar", lambda _t, _ws: None,
    )
    monkeypatch.setattr(
        cli_mod, "_wait_for_peer_ready",
        lambda _ws, team_name="", agents=None: set(),
    )

    # Dispatch stubs
    monkeypatch.setattr(
        cli_mod, "_request_send_payload",
        lambda **_k: None,
    )

    # Task artifact file
    task = tmp_path / "task.md"
    task.write_text("# task")

    result = runner.invoke(
        cli,
        ["squad", "spawn-duo", "--feature-id", "role-config", "--task", str(task)],
    )
    assert result.exit_code == 0, result.output

    # Worker spawn
    assert len(spawned) == 2
    worker_spawn = spawned[0]
    assert worker_spawn["name"] == "peaky.worker-1000"
    assert worker_spawn["cli"] == "claude"
    assert worker_spawn["model"] == "opus"

    # Validator spawn
    validator_spawn = spawned[1]
    assert validator_spawn["name"] == "peaky.validator-1000"
    assert validator_spawn["cli"] == "codex"
    assert validator_spawn["model"] == "o3"


def test_squad_spawn_duo_validator_fallback_when_no_role_cli(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """When roles.validator.cli is unset, validator CLI falls back to
    anti_peer_cli(worker_cli) while model still applies."""
    configure_hive_home(current_pane="%100", session_name="dev")

    import hive.cli as cli_mod
    from hive.agent import Agent
    from hive.team import Team
    from types import SimpleNamespace

    # Only validator model set, no CLI
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.model": "o3",
        }.get(key, default),
    )

    monkeypatch.setattr(
        cli_mod.tmux, "get_pane_option",
        lambda pane, key: {
            "hive-group": "peaky",
            "hive-cli": "claude",
        }.get(key, ""),
    )
    monkeypatch.setattr(
        cli_mod.tmux, "get_window_option",
        lambda win, key: {
            "hive-squad-base": "1000",
            "hive-squad-name": "peaky",
        }.get(key, ""),
    )

    fake_team = Team(
        name="dev-w0",
        workspace=str(tmp_path / "ws"),
        tmux_session="dev",
        tmux_window="dev:0",
        tmux_window_id="@0",
    )
    (tmp_path / "ws").mkdir()
    monkeypatch.setattr(
        cli_mod, "_resolve_scoped_team",
        lambda _team, required=True: ("dev-w0", fake_team),
    )

    monkeypatch.setattr(cli_mod.tmux, "list_window_indices", lambda _s: [0])
    monkeypatch.setattr(cli_mod.tmux, "list_panes_all", lambda: [])
    monkeypatch.setattr(cli_mod.tmux, "list_panes", lambda _w: ["%200"])
    monkeypatch.setattr(
        cli_mod.tmux, "new_window",
        lambda _s, name="", cwd="", index=0: (f"dev:{index}", "%200"),
    )
    monkeypatch.setattr(cli_mod.tmux, "set_window_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "configure_hive_window", lambda _t: None)
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda _t, _n: None)
    monkeypatch.setattr(cli_mod.tmux, "get_window_id", lambda _t: "@99")
    monkeypatch.setattr(
        cli_mod.tmux, "display_value",
        lambda _p, _f: str(tmp_path),
    )
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _t, _c: True)
    monkeypatch.setattr(
        "hive.layout.apply_adaptive",
        lambda _t: SimpleNamespace(orientation="horizontal"),
    )

    spawned: list[dict] = []

    def fake_spawn(**kwargs):
        spawned.append(kwargs)
        return Agent(
            name=str(kwargs["name"]),
            team_name=str(kwargs["team_name"]),
            pane_id=f"%{200 + len(spawned)}",
            cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))
    monkeypatch.setattr(cli_mod, "_ensure_team_sidecar", lambda _t, _ws: None)
    monkeypatch.setattr(
        cli_mod, "_wait_for_peer_ready",
        lambda _ws, team_name="", agents=None: set(),
    )
    monkeypatch.setattr(cli_mod, "_request_send_payload", lambda **_k: None)

    task = tmp_path / "task.md"
    task.write_text("# task")

    result = runner.invoke(
        cli,
        ["squad", "spawn-duo", "--feature-id", "role-config", "--task", str(task)],
    )
    assert result.exit_code == 0, result.output

    assert len(spawned) == 2
    # Worker: no role config → orch's CLI fallback (claude)
    assert spawned[0]["cli"] == "claude"
    assert spawned[0]["model"] == ""

    # Validator: CLI falls back to anti_peer_cli("claude") = codex, model from config
    assert spawned[1]["cli"] == "codex"
    assert spawned[1]["model"] == "o3"


# --- --base integration branch propagation ---


def _setup_spawn_harness(monkeypatch, tmp_path, *, orch_integration=""):
    """Wire up a full spawn-duo harness, returning (task_path, window_opts).

    window_opts tracks all set_window_option calls as {target: {key: value}}.
    If orch_integration is non-empty, seed the orch window with that value.
    """
    import hive.cli as cli_mod
    from hive.agent import Agent
    from hive.team import Team
    from types import SimpleNamespace

    monkeypatch.setattr(
        "hive.settings.get_setting", lambda key, default=None: default,
    )

    window_opts: dict[str, dict[str, str]] = {}

    def track_set_window_option(target, option, value=""):
        key = option.removeprefix("@")
        window_opts.setdefault(target, {})[key] = value

    def track_get_window_option(target, key):
        return window_opts.get(target, {}).get(key)

    if orch_integration:
        window_opts.setdefault("dev:0", {})["hive-squad-integration-branch"] = orch_integration

    monkeypatch.setattr(
        cli_mod.tmux, "get_pane_option",
        lambda pane, key: {"hive-group": "peaky", "hive-cli": "claude"}.get(key, ""),
    )
    monkeypatch.setattr(cli_mod.tmux, "get_window_option", track_get_window_option)
    monkeypatch.setattr(cli_mod.tmux, "set_window_option", track_set_window_option)

    fake_team = Team(
        name="dev-w0", workspace=str(tmp_path / "ws"),
        tmux_session="dev", tmux_window="dev:0", tmux_window_id="@0",
    )
    (tmp_path / "ws").mkdir(exist_ok=True)
    monkeypatch.setattr(
        cli_mod, "_resolve_scoped_team",
        lambda _team, required=True: ("dev-w0", fake_team),
    )

    monkeypatch.setattr(cli_mod.tmux, "list_window_indices", lambda _s: [0])
    monkeypatch.setattr(cli_mod.tmux, "list_panes_all", lambda: [])
    monkeypatch.setattr(cli_mod.tmux, "list_panes", lambda _w: ["%200"])
    monkeypatch.setattr(
        cli_mod.tmux, "new_window",
        lambda _s, name="", cwd="", index=0: (f"dev:{index}", "%200"),
    )
    monkeypatch.setattr(cli_mod.tmux, "set_pane_option", lambda *_a: None)
    monkeypatch.setattr(cli_mod.tmux, "configure_hive_window", lambda _t: None)
    monkeypatch.setattr(cli_mod.tmux, "rename_window", lambda _t, _n: None)
    monkeypatch.setattr(cli_mod.tmux, "get_window_id", lambda _t: "@99")
    monkeypatch.setattr(cli_mod.tmux, "display_value", lambda _p, _f: str(tmp_path))
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _t, _c: True)
    monkeypatch.setattr(
        "hive.layout.apply_adaptive",
        lambda _t: SimpleNamespace(orientation="horizontal"),
    )

    def fake_spawn(**kwargs):
        return Agent(
            name=str(kwargs["name"]), team_name=str(kwargs["team_name"]),
            pane_id=f"%{200}", cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr(cli_mod.Agent, "spawn", staticmethod(fake_spawn))
    monkeypatch.setattr(cli_mod, "_ensure_team_sidecar", lambda _t, _ws: None)
    monkeypatch.setattr(
        cli_mod, "_wait_for_peer_ready",
        lambda _ws, team_name="", agents=None: set(),
    )
    monkeypatch.setattr(cli_mod, "_request_send_payload", lambda **_k: None)

    task = tmp_path / "task.md"
    task.write_text("# task")
    return task, window_opts


def test_spawn_duo_base_sets_integration_on_both_windows(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """--base stamps @hive-squad-integration-branch on orch + duo windows
    and JSON output includes integrationBranch."""
    configure_hive_home(current_pane="%100", session_name="dev")
    task, window_opts = _setup_spawn_harness(monkeypatch, tmp_path)

    monkeypatch.setattr(
        "hive.worktree.repo_anchor", lambda _cwd: tmp_path,
    )
    monkeypatch.setattr(
        "hive.worktree.rev_parse", lambda _anchor, ref: "abc123",
    )

    result = runner.invoke(
        cli,
        ["squad", "spawn-duo", "--feature-id", "test-base", "--task", str(task),
         "--base", "peaky-integration"],
    )
    assert result.exit_code == 0, result.output

    import json
    out = json.loads(result.output)
    assert out["integrationBranch"] == "peaky-integration"
    assert "integrationWarning" not in out

    assert window_opts.get("dev:0", {}).get("hive-squad-integration-branch") == "peaky-integration"
    assert window_opts.get("dev:1000", {}).get("hive-squad-integration-branch") == "peaky-integration"


def test_spawn_duo_invalid_base_fails_before_side_effects(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Invalid --base must fail before new_window / set_window_option / Agent.spawn."""
    configure_hive_home(current_pane="%100", session_name="dev")

    import hive.cli as cli_mod
    from hive.worktree import WorktreeError

    monkeypatch.setattr(
        cli_mod.tmux, "get_pane_option",
        lambda pane, key: {"hive-group": "peaky", "hive-cli": "claude"}.get(key, ""),
    )
    monkeypatch.setattr(
        "hive.worktree.repo_anchor", lambda _cwd: tmp_path,
    )
    monkeypatch.setattr(
        "hive.worktree.rev_parse",
        lambda _anchor, ref: (_ for _ in ()).throw(WorktreeError(f"cannot resolve ref '{ref}'")),
    )

    for fn in ("new_window", "set_window_option", "rename_window"):
        monkeypatch.setattr(f"hive.cli.tmux.{fn}", _explode(fn))

    task = tmp_path / "task.md"
    task.write_text("# task")

    result = runner.invoke(
        cli,
        ["squad", "spawn-duo", "--feature-id", "test-base", "--task", str(task),
         "--base", "nonexistent-branch"],
    )
    assert result.exit_code == 1
    assert "--base nonexistent-branch" in result.output


def test_spawn_duo_no_base_copies_existing_integration(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Without --base, existing @hive-squad-integration-branch on orch window
    is copied to duo window."""
    configure_hive_home(current_pane="%100", session_name="dev")
    task, window_opts = _setup_spawn_harness(
        monkeypatch, tmp_path, orch_integration="peaky-integration",
    )

    result = runner.invoke(
        cli,
        ["squad", "spawn-duo", "--feature-id", "test-copy", "--task", str(task)],
    )
    assert result.exit_code == 0, result.output

    import json
    out = json.loads(result.output)
    assert out["integrationBranch"] == "peaky-integration"
    assert "integrationWarning" not in out

    assert window_opts.get("dev:1000", {}).get("hive-squad-integration-branch") == "peaky-integration"


def test_spawn_duo_no_base_no_existing_returns_warning(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Without --base and no existing integration branch, JSON includes
    integrationBranch: null and integrationWarning."""
    configure_hive_home(current_pane="%100", session_name="dev")
    task, window_opts = _setup_spawn_harness(monkeypatch, tmp_path)

    result = runner.invoke(
        cli,
        ["squad", "spawn-duo", "--feature-id", "test-warn", "--task", str(task)],
    )
    assert result.exit_code == 0, result.output

    import json
    out = json.loads(result.output)
    assert out["integrationBranch"] is None
    assert "integrationWarning" in out
    assert "hive worktree start" in out["integrationWarning"]
