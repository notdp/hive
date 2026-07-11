import json
from types import SimpleNamespace

from hive import resume as resume_store
from hive.agent import Agent
from hive.cli import cli
from hive.tmux import PaneInfo


def _save_snap(handle="0-w2", *, cwd="/repo", **over):
    members = over.pop("members", None) or [
        {"name": "worker", "cli": "claude", "model": "m1", "sessionId": "sid-w", "cwd": cwd},
        {"name": "validator", "cli": "codex", "model": "m2", "sessionId": "sid-v", "cwd": cwd},
    ]
    snap = resume_store.build_snapshot(
        handle=handle,
        team=over.pop("team", handle),
        group=over.pop("group", "duo"),
        window_name=over.pop("window_name", "hive"),
        workspace=over.pop("workspace", "/tmp/ws"),
        repo_cwd=cwd,
        branch="main",
        created_at=over.pop("created_at", "100.0"),
        members=members,
    )
    snap.update(over)
    assert resume_store.save_snapshot(snap, now="2026-07-10T00:00:00Z") == "written"
    return snap


def _pane(pane_id, team, agent, cli_name):
    return PaneInfo(pane_id, f"[{agent}]", "node", role="agent", agent=agent, team=team, cli=cli_name)


def _tmux_state(monkeypatch, *, panes="ok", windows="ok", pane_list=None, window_list=None):
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_all_status",
        lambda: (pane_list if panes == "ok" else None, panes if panes != "ok" else "ok"),
    )
    monkeypatch.setattr(
        "hive.cli.tmux.list_team_windows_status",
        lambda: (window_list if windows == "ok" else None, windows if windows != "ok" else "ok"),
    )
    # Live-context enrichment must never touch the real tmux/git in tests.
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _p, _fmt: "/live/repo-cwd")
    monkeypatch.setattr("hive.resume.repo_label", lambda cwd: "liverepo" if cwd else "")
    monkeypatch.setattr("hive.resume.git_branch", lambda cwd: "livebranch" if cwd else "")


# --- hive ls (VAL C8-C9) ---


def test_ls_json_merges_live_and_snapshots(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _save_snap("dead1")
    _save_snap("0-w2")
    (tmp_path / ".hive" / "state" / "resume" / "broken.json").write_text("{nope")

    _tmux_state(
        monkeypatch,
        pane_list=[
            _pane("%1", "0-w2", "worker", "claude"),  # validator missing → incomplete
            _pane("%5", "0-w9", "worker", "codex"),
            _pane("%6", "0-w9", "validator", "claude"),
        ],
        window_list=[
            {"window": "dev:2", "windowName": "hive", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0"},
            {"window": "dev:9", "windowName": "other", "windowId": "@9", "team": "0-w9", "workspace": "/tmp/w9", "created": "50.0"},
        ],
    )

    result = runner.invoke(cli, ["ls", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["tmux"] == "ok"
    by_key = {(e.get("handle") or e.get("team")): e for e in payload["teams"]}

    dead = by_key["dead1"]
    assert dead["state"] == "restorable"
    assert dead["savedAt"] and dead["branch"] == "main"
    assert {m["name"] for m in dead["members"]} == {"worker", "validator"}
    assert all(m["session"] for m in dead["members"])

    incomplete = by_key["0-w2"]
    assert incomplete["state"] == "live-incomplete"
    assert incomplete["window"] == "dev:2" and incomplete["handle"] == "0-w2"
    live_flags = {m["name"]: m["live"] for m in incomplete["members"]}
    assert live_flags == {"worker": True, "validator": False}

    live_only = by_key["0-w9"]
    assert live_only["state"] == "live-complete" and live_only["window"] == "dev:9"

    assert by_key["broken"]["state"] == "corrupt"
    # one row per team — the live 0-w2 merged into its snapshot entry
    assert len([e for e in payload["teams"] if (e.get("team") or e.get("handle")) == "0-w2"]) == 1


def test_ls_unknown_tmux_is_not_reported_dead(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _save_snap("0-w2")
    _tmux_state(monkeypatch, panes="unknown", windows="ok", window_list=[])

    result = runner.invoke(cli, ["ls", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["tmux"] == "unknown"
    assert payload["teams"][0]["state"] == "unknown"  # not "restorable", not "live"


def test_ls_no_server_lists_snapshots_as_restorable(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _save_snap("0-w2")
    _tmux_state(monkeypatch, panes="no-server", windows="no-server")

    result = runner.invoke(cli, ["ls", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["tmux"] == "no-server"
    assert payload["teams"][0]["state"] == "restorable"


def test_ls_marks_superseded_when_live_instance_differs(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _save_snap("0-w2", created_at="100.0")
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%1", "0-w2", "worker", "claude")],
        window_list=[{"window": "dev:2", "windowName": "hive", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "999.0"}],
    )

    result = runner.invoke(cli, ["ls", "--json"])
    payload = json.loads(result.output)
    assert payload["teams"][0]["state"] == "superseded"


# --- hive resume shared mocks ---


def _resume_mocks(monkeypatch, *, team_obj="default"):
    rec = SimpleNamespace(
        spawns=[], new_windows=[], killed_windows=[], killed_panes=[],
        peers=[], layouts=[], sidecars=[], selects=[], contexts=[],
    )
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.set_pane_option", lambda *_a: None)
    monkeypatch.setattr("hive.cli.tmux.select_window", lambda t: rec.selects.append(t))
    monkeypatch.setattr("hive.cli.tmux.kill_window", lambda t: rec.killed_windows.append(t))
    monkeypatch.setattr("hive.cli.tmux.kill_pane", lambda p: rec.killed_panes.append(p))
    monkeypatch.setattr(
        "hive.cli.tmux.new_window",
        lambda session, name="", cwd=None, detach=True, index=None: rec.new_windows.append(
            {"session": session, "name": name, "cwd": cwd, "detach": detach}
        ) or ("dev:7", "%70"),
    )
    monkeypatch.setattr("hive.cli.hive_context.save_context_for_pane", lambda pane, **kw: rec.contexts.append((pane, kw)))
    monkeypatch.setattr("hive.layout.split_horizontal", lambda _t, _c: True)
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda t: rec.layouts.append(t))
    monkeypatch.setattr("hive.cli._prepare_window_for_new_team", lambda *_a, **_k: None)
    monkeypatch.setattr("hive.cli._claim_team_name", lambda *_a, **_k: None)
    monkeypatch.setattr("hive.cli.bus.init_workspace", lambda _ws: None)
    monkeypatch.setattr("hive.cli._ensure_team_sidecar", lambda t, ws: rec.sidecars.append((t.name, str(ws))) or 1)

    if team_obj == "default":
        team_obj = SimpleNamespace(
            name="0-w2",
            created_at=555.0,
            agents={"worker": SimpleNamespace(pane_id="%71"), "validator": SimpleNamespace(pane_id="%72")},
            peer_map={},
            set_peer=lambda a, b: rec.peers.append((a, b)),
        )
    monkeypatch.setattr("hive.cli.Team.create_for_window", staticmethod(lambda name, **kw: team_obj))
    monkeypatch.setattr("hive.cli.Team.load", staticmethod(lambda name, prefer_pane="": team_obj))

    pane_seq = iter(["%71", "%72", "%73"])

    def fake_spawn(**kwargs):
        rec.spawns.append(kwargs)
        return Agent(
            name=str(kwargs["name"]), team_name=str(kwargs["team_name"]),
            pane_id=next(pane_seq), cli=str(kwargs["cli"]),
        )

    monkeypatch.setattr("hive.cli.Agent.spawn", staticmethod(fake_spawn))
    return rec


def _assert_zero_mutation(rec):
    assert rec.spawns == []
    assert rec.new_windows == []
    assert rec.killed_windows == []
    assert rec.killed_panes == []
    assert rec.selects == []  # never switch the human's window before preflight passes


# --- hive resume prechecks (VAL D10) ---


def test_resume_precheck_failures_never_mutate(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _tmux_state(monkeypatch, pane_list=[], window_list=[])
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()

    cases = []
    # no handle / unknown handle
    cases.append((["resume"], "hive ls"))
    cases.append((["resume", "nope"], "no usable snapshot"))
    # unsupported group
    _save_snap("sq", group="squad", cwd=good_cwd)
    cases.append((["resume", "sq"], "duo only"))
    # corrupt schema
    (tmp_path / ".hive" / "state" / "resume" / "bad.json").write_text("{nope")
    cases.append((["resume", "bad"], "no usable snapshot"))
    # missing sessionId for the worker — hard failure naming the worker only
    # (a validator-only miss degrades to a fresh spawn instead of failing)
    _save_snap("nosess", cwd=good_cwd, members=[
        {"name": "worker", "cli": "claude", "sessionId": "", "cwd": good_cwd},
        {"name": "validator", "cli": "codex", "sessionId": "sid-v", "cwd": good_cwd},
    ])
    cases.append((["resume", "nosess"], "missing sessionId for worker"))
    # missing cwd on disk
    _save_snap("nocwd", cwd=str(tmp_path / "gone"))
    cases.append((["resume", "nocwd"], "missing on disk"))
    # a validator WITH a saved session keeps its own cwd requirement
    _save_snap("vcwd", cwd=good_cwd, members=[
        {"name": "worker", "cli": "claude", "sessionId": "sid-w", "cwd": good_cwd},
        {"name": "validator", "cli": "codex", "sessionId": "sid-v", "cwd": str(tmp_path / "gone-v")},
    ])
    cases.append((["resume", "vcwd"], "missing on disk"))

    for args, needle in cases:
        result = runner.invoke(cli, args)
        assert result.exit_code != 0, (args, result.output)
        assert needle in result.output, (args, result.output)
    _assert_zero_mutation(rec)


def test_resume_fails_before_mutation_on_unknown_tmux_listing(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    _save_snap("0-w2", cwd=good_cwd)
    _tmux_state(monkeypatch, panes="unknown", windows="ok", window_list=[])

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert "did not answer" in result.output
    _assert_zero_mutation(rec)


# --- live team paths (VAL D11-D12) ---


def _live_setup(monkeypatch, tmp_path, *, live_panes, created="100.0"):
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir(exist_ok=True)
    snap = _save_snap("0-w2", cwd=good_cwd, created_at="100.0")
    _tmux_state(
        monkeypatch,
        pane_list=live_panes,
        window_list=[{
            "window": "dev:2", "windowName": "hive", "windowId": "@2",
            "team": "0-w2", "workspace": "/tmp/ws", "created": created,
        }],
    )
    return snap, good_cwd


def test_resume_live_complete_is_zero_mutation_error(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _live_setup(monkeypatch, tmp_path, live_panes=[
        _pane("%1", "0-w2", "worker", "claude"),
        _pane("%2", "0-w2", "validator", "codex"),
    ])

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert "live and complete" in result.output
    _assert_zero_mutation(rec)


def test_resume_revives_missing_validator_with_session(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _, good_cwd = _live_setup(monkeypatch, tmp_path, live_panes=[_pane("%1", "0-w2", "worker", "claude")])

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)

    assert payload["resumed"] == "members"
    assert len(rec.spawns) == 1
    spawn = rec.spawns[0]
    assert spawn["name"] == "validator" and spawn["cli"] == "codex"
    assert spawn["session_id"] == "sid-v" and spawn["session_mode"] == "resume"
    assert spawn["cwd"] == good_cwd and spawn["workspace"] == "/tmp/ws"
    assert spawn["skill"] == "none"
    assert rec.peers == [("worker", "validator")]
    assert rec.layouts == ["dev:2"]
    assert rec.new_windows == []  # revived into the existing window
    assert rec.selects == ["dev:2"]  # human is taken to the team window


def test_resume_revives_missing_worker_too(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _live_setup(monkeypatch, tmp_path, live_panes=[_pane("%2", "0-w2", "validator", "codex")])

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    assert len(rec.spawns) == 1
    assert rec.spawns[0]["name"] == "worker"
    assert rec.spawns[0]["session_id"] == "sid-w"
    assert rec.spawns[0]["session_mode"] == "resume"


def test_resume_member_failure_cleans_only_new_panes(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _live_setup(monkeypatch, tmp_path, live_panes=[_pane("%1", "0-w2", "worker", "claude")])

    def failing_load(name, prefer_pane=""):
        raise ValueError("peer wiring failed")

    monkeypatch.setattr("hive.cli.Team.load", staticmethod(failing_load))

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert rec.killed_panes == ["%71"]  # only the freshly spawned pane
    assert rec.killed_windows == []  # survivors' window untouched
    assert resume_store.load_snapshot("0-w2") is not None  # snapshot kept


def test_resume_refuses_mismatched_live_identity(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)

    # live roster has a stranger
    _live_setup(monkeypatch, tmp_path, live_panes=[
        _pane("%1", "0-w2", "worker", "claude"),
        _pane("%9", "0-w2", "intruder", "codex"),
    ])
    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0 and "not in the snapshot" in result.output

    # live member runs a different cli than the snapshot recorded
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%1", "0-w2", "worker", "codex")],
        window_list=[{"window": "dev:2", "windowName": "hive", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0"}],
    )
    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0 and "cli differs" in result.output

    # a different live instance owns the team name
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%1", "0-w2", "worker", "claude")],
        window_list=[{"window": "dev:2", "windowName": "hive", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "999.0"}],
    )
    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0 and "superseded" in result.output

    _assert_zero_mutation(rec)


# --- full restore (VAL D13-D14) ---


def _dead_setup(monkeypatch, tmp_path):
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir(exist_ok=True)
    snap = _save_snap("0-w2", cwd=good_cwd, workspace=str(tmp_path / "ws"))
    _tmux_state(monkeypatch, pane_list=[], window_list=[])
    return snap, good_cwd


def test_resume_full_restore_rebuilds_team_in_new_window(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _, good_cwd = _dead_setup(monkeypatch, tmp_path)

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)

    assert payload["resumed"] == "full" and payload["window"] == "dev:7"
    assert rec.new_windows == [{"session": "dev", "name": "hive", "cwd": good_cwd, "detach": True}]

    assert len(rec.spawns) == 2
    first, second = rec.spawns
    # worker takes over the new window's shell pane in place; validator splits
    assert first["name"] == "worker" and first["split_window"] is False
    assert first["target_pane"] == "%70"
    assert first["session_id"] == "sid-w" and first["session_mode"] == "resume"
    assert first["cwd"] == good_cwd
    assert second["name"] == "validator" and second.get("split_window", True) is True
    assert second["session_id"] == "sid-v" and second["session_mode"] == "resume"

    assert rec.peers == [("worker", "validator")]
    assert rec.sidecars and rec.sidecars[0][0] == "0-w2"
    # The human asked for the team: resume switches them to the new window.
    assert rec.selects == ["dev:7"]
    assert rec.killed_windows == []

    # continuation snapshot: new instance identity, no .prev archive
    snap = resume_store.load_snapshot("0-w2")
    assert snap["createdAt"] == "555.0"
    assert resume_store.load_snapshot("0-w2.prev") is None


def test_resume_full_restore_rolls_back_on_second_spawn_failure(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _dead_setup(monkeypatch, tmp_path)

    calls = {"n": 0}

    def flaky_spawn(**kwargs):
        calls["n"] += 1
        if calls["n"] == 2:
            raise RuntimeError("codex daemon failed to bind")
        rec.spawns.append(kwargs)
        return Agent(name=str(kwargs["name"]), team_name="0-w2", pane_id="%71", cli=str(kwargs["cli"]))

    monkeypatch.setattr("hive.cli.Agent.spawn", staticmethod(flaky_spawn))

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert rec.killed_windows == ["dev:7"]  # whole new window rolled back
    snap = resume_store.load_snapshot("0-w2")
    assert snap is not None and snap["createdAt"] == "100.0"  # snapshot untouched, retryable


def test_resume_full_restore_rolls_back_on_peer_and_sidecar_failure(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()

    # peer failure
    rec = _resume_mocks(monkeypatch)
    _dead_setup(monkeypatch, tmp_path)
    bad_peer = SimpleNamespace(
        name="0-w2", created_at=555.0,
        agents={"worker": SimpleNamespace(pane_id="%71"), "validator": SimpleNamespace(pane_id="%72")},
        set_peer=lambda a, b: (_ for _ in ()).throw(KeyError("peer")),
    )
    monkeypatch.setattr("hive.cli.Team.load", staticmethod(lambda name, prefer_pane="": bad_peer))
    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert rec.killed_windows == ["dev:7"]
    assert resume_store.load_snapshot("0-w2")["createdAt"] == "100.0"

    # sidecar failure (starts last, after the continuation snapshot commit:
    # the window still rolls back and the snapshot stays retryable)
    rec2 = _resume_mocks(monkeypatch)
    def bad_sidecar(t, ws):
        raise RuntimeError("sidecar spawn failed")
    monkeypatch.setattr("hive.cli._ensure_team_sidecar", bad_sidecar)
    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert rec2.killed_windows == ["dev:7"]
    assert resume_store.load_snapshot("0-w2") is not None  # retryable


def test_ls_works_outside_tmux(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    _save_snap("0-w2")
    _tmux_state(monkeypatch, panes="no-server", windows="no-server")

    result = runner.invoke(cli, ["ls", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["teams"][0]["state"] == "restorable"


def test_resume_requires_tmux_session(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["resume", "whatever"])
    assert result.exit_code != 0


# --- round-2 regressions (validator findings) ---


def test_ls_superseded_snapshot_does_not_hide_live_team(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _save_snap("0-w2", created_at="100.0")
    _tmux_state(
        monkeypatch,
        pane_list=[
            _pane("%1", "0-w2", "worker", "claude"),
            _pane("%2", "0-w2", "validator", "codex"),
        ],
        window_list=[{"window": "dev:2", "windowName": "hive", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "999.0"}],
    )

    result = runner.invoke(cli, ["ls", "--json"])
    payload = json.loads(result.output)
    rows = [e for e in payload["teams"] if (e.get("team") or e.get("handle")) == "0-w2"]
    states = sorted(str(e["state"]) for e in rows)
    assert states == ["live-complete", "superseded"]
    live_row = next(e for e in rows if e["state"] == "live-complete")
    assert live_row["window"] == "dev:2"


def test_resume_rejects_non_duo_roster_shape(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _tmux_state(monkeypatch, pane_list=[], window_list=[])
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    _save_snap("odd", cwd=good_cwd, members=[
        {"name": "worker", "cli": "claude", "sessionId": "s1", "cwd": good_cwd},
        {"name": "observer", "cli": "codex", "sessionId": "s2", "cwd": good_cwd},
    ])

    result = runner.invoke(cli, ["resume", "odd"])
    assert result.exit_code != 0
    assert "worker + validator" in result.output
    _assert_zero_mutation(rec)


def test_resume_member_tag_failure_kills_fresh_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _live_setup(monkeypatch, tmp_path, live_panes=[_pane("%1", "0-w2", "worker", "claude")])

    def bad_tag(*_a):
        raise RuntimeError("tag failed")

    monkeypatch.setattr("hive.cli.tmux.set_pane_option", bad_tag)

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert rec.killed_panes == ["%71"]  # pane tracked from birth, not after tagging
    assert rec.killed_windows == []


def test_resume_member_layout_failure_restores_prior_peer_map(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _live_setup(monkeypatch, tmp_path, live_panes=[_pane("%1", "0-w2", "worker", "claude")])

    saves: list[dict] = []
    team = SimpleNamespace(
        name="0-w2",
        created_at=100.0,
        agents={"worker": SimpleNamespace(pane_id="%1"), "validator": SimpleNamespace(pane_id="%71")},
        peer_map={},
    )

    def fake_set_peer(a, b):
        team.peer_map = {a: b, b: a}

    team.set_peer = fake_set_peer
    team.save = lambda: saves.append(dict(team.peer_map))
    monkeypatch.setattr("hive.cli.Team.load", staticmethod(lambda name, prefer_pane="": team))

    def bad_layout(_t):
        raise RuntimeError("layout failed")

    monkeypatch.setattr("hive.layout.apply_adaptive", bad_layout)

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert rec.killed_panes == ["%71"]
    assert team.peer_map == {}  # prior (empty) peer state restored
    assert saves and saves[-1] == {}


def test_resume_full_restore_commits_snapshot_before_sidecar(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _dead_setup(monkeypatch, tmp_path)

    order: list[str] = []
    real_save = resume_store.save_snapshot

    def spy_save(snap, *, now, archive_on_new_instance=True):
        order.append("save")
        return real_save(snap, now=now, archive_on_new_instance=archive_on_new_instance)

    monkeypatch.setattr("hive.resume.save_snapshot", spy_save)
    monkeypatch.setattr(
        "hive.cli._ensure_team_sidecar",
        lambda t, ws: order.append("sidecar") or 1,
    )

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    assert order == ["save", "sidecar"]  # identity committed before the sidecar can race


# --- hive ls enrichment + grouped output (design D) ---


def test_ls_json_live_rows_carry_repo_context(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _save_snap("0-w2", window_name="pair")
    _tmux_state(
        monkeypatch,
        pane_list=[
            _pane("%1", "0-w2", "worker", "claude"),
            _pane("%2", "0-w2", "validator", "codex"),
            _pane("%5", "0-w9", "worker", "codex"),
        ],
        window_list=[
            {"window": "dev:3", "windowName": "pair", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0", "pr": "52"},
            {"window": "dev:9", "windowName": "other", "windowId": "@9", "team": "0-w9", "workspace": "/tmp/w9", "created": "50.0", "pr": ""},
        ],
    )

    result = runner.invoke(cli, ["ls", "--json"])
    assert result.exit_code == 0, result.output
    by_team = {e["team"]: e for e in json.loads(result.output)["teams"]}

    merged = by_team["0-w2"]
    assert merged["state"] == "live-complete"
    assert merged["window"] == "dev:3" and merged["windowName"] == "pair"
    assert merged["pr"] == "52"
    # live truth overrides whatever the snapshot recorded
    assert merged["repo"] == "liverepo" and merged["branch"] == "livebranch"
    assert merged["repoCwd"] == "/live/repo-cwd"
    assert [m["name"] for m in merged["members"]][0] == "worker"  # worker-first

    live_only = by_team["0-w9"]
    assert live_only["repo"] == "liverepo" and live_only["windowName"] == "other"


def test_ls_anchor_prefers_worker_then_orch_then_first(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    import hive.cli as cli_mod
    from hive.cli import _live_anchor_pane

    worker, orch, zeta = _pane("%1", "t", "worker", "claude"), _pane("%2", "t", "orch", "claude"), _pane("%3", "t", "zeta", "codex")
    assert _live_anchor_pane({"worker": worker, "orch": orch, "zeta": zeta}).pane_id == "%1"
    assert _live_anchor_pane({"orch": orch, "zeta": zeta}).pane_id == "%2"
    assert _live_anchor_pane({"zeta": zeta, "alpha": _pane("%4", "t", "alpha", "codex")}).pane_id == "%4"


def test_ls_human_output_is_grouped(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _save_snap("0-w5")  # restorable
    _save_snap("0-w2", window_name="pair")  # live-incomplete (validator dead)
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%1", "0-w2", "worker", "claude")],
        window_list=[{"window": "dev:3", "windowName": "pair", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0", "pr": "52"}],
    )

    result = runner.invoke(cli, ["ls", "--plain"])
    assert result.exit_code == 0, result.output
    out = result.output
    # structural facts, not prose: group headers in order, one per section
    assert out.index("LIVE") < out.index("RESTORABLE")
    live_line = next(l for l in out.splitlines() if "dev:3" in l)
    assert "liverepo @ livebranch" in live_line and "PR#52" in live_line
    sid_w2 = resume_store.short_id({"handle": "0-w2", "createdAt": "100.0"})
    assert "missing validator" in live_line and f"hive resume {sid_w2}" in live_line
    dead_line = next(l for l in out.splitlines() if "0-w5" in l and "resume" in l)
    sid_w5 = resume_store.short_id({"handle": "0-w5", "createdAt": "100.0"})
    assert f"hive resume {sid_w5}" in dead_line and ("ago" in dead_line or "?" in dead_line)
    assert "OTHER" not in out  # no unknown/corrupt rows → no group


def test_ls_human_groups_other_states_separately(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    (tmp_path / ".hive" / "state" / "resume").mkdir(parents=True)
    (tmp_path / ".hive" / "state" / "resume" / "broken.json").write_text("{nope")
    _tmux_state(monkeypatch, pane_list=[], window_list=[])

    result = runner.invoke(cli, ["ls", "--plain"])
    assert result.exit_code == 0, result.output
    assert "OTHER" in result.output
    assert "LIVE" not in result.output and "RESTORABLE" not in result.output


def test_ls_live_overlay_clears_stale_snapshot_pr(runner, configure_hive_home, monkeypatch):
    """`hive duo clear-pr` removed the stamp: live empty pr must win over snapshot."""
    configure_hive_home()
    snap = _save_snap("0-w2")
    snap["pr"] = "52"
    assert resume_store.save_snapshot(snap, now="t2") == "written"
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%1", "0-w2", "worker", "claude"), _pane("%2", "0-w2", "validator", "codex")],
        window_list=[{"window": "dev:3", "windowName": "pair", "windowId": "@2", "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0", "pr": ""}],
    )

    result = runner.invoke(cli, ["ls", "--json"])
    entry = next(e for e in json.loads(result.output)["teams"] if e["team"] == "0-w2")
    assert entry["pr"] == ""
    assert "PR#" not in runner.invoke(cli, ["ls"]).output


# --- resume progress feedback (stderr) ---


def test_resume_progress_streams_to_stderr_stdout_stays_json(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    _resume_mocks(monkeypatch)
    _dead_setup(monkeypatch, tmp_path)

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.stdout)  # stdout parses standalone
    assert payload["resumed"] == "full"
    assert "resuming" not in result.stdout and "ready in" not in result.stdout
    err = result.stderr
    assert "window dev:7" in err
    for name, pane in (("worker", "%71"), ("validator", "%72")):
        assert f"resuming {name}" in err
        assert f"{name} ready in {pane}" in err


def test_resume_member_progress_has_no_window_line(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    _resume_mocks(monkeypatch)
    _live_setup(monkeypatch, tmp_path, live_panes=[_pane("%1", "0-w2", "worker", "claude")])

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["resumed"] == "members"
    err = result.stderr
    assert "window" not in err  # member revive reuses the live window
    assert "resuming validator" in err and "validator ready in %71" in err


def test_resume_progress_ordering_locked_by_call_log(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Progress must precede each spawn and follow each return — order, not
    just final stderr content."""
    configure_hive_home()
    import hive.cli as cli_mod

    rec = _resume_mocks(monkeypatch)
    _dead_setup(monkeypatch, tmp_path)

    log: list[str] = []
    monkeypatch.setattr(cli_mod, "_resume_progress", lambda msg: log.append(f"progress:{msg.split(' ')[0]}:{msg.split(' ')[1] if ' ' in msg else ''}"))
    monkeypatch.setattr("hive.cli.tmux.select_window", lambda t: log.append(f"select:{t}"))

    pane_seq = iter(["%71", "%72"])

    def logging_spawn(**kwargs):
        log.append(f"spawn:{kwargs['name']}")
        rec.spawns.append(kwargs)
        return Agent(name=str(kwargs["name"]), team_name="0-w2", pane_id=next(pane_seq), cli=str(kwargs["cli"]))

    monkeypatch.setattr("hive.cli.Agent.spawn", staticmethod(logging_spawn))

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    assert log == [
        "progress:window:dev:7",
        "select:dev:7",
        "progress:resuming:worker",
        "spawn:worker",
        "progress:worker:ready",
        "progress:resuming:validator",
        "spawn:validator",
        "progress:validator:ready",
    ]


def test_resume_failure_keeps_stage_hints_and_no_success_json(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _dead_setup(monkeypatch, tmp_path)

    calls = {"n": 0}

    def flaky_spawn(**kwargs):
        calls["n"] += 1
        if calls["n"] == 2:
            raise RuntimeError("codex daemon failed to bind")
        rec.spawns.append(kwargs)
        return Agent(name=str(kwargs["name"]), team_name="0-w2", pane_id="%71", cli=str(kwargs["cli"]))

    monkeypatch.setattr("hive.cli.Agent.spawn", staticmethod(flaky_spawn))

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert "resumed" not in result.stdout  # no phantom success JSON
    err = result.stderr
    assert "resuming worker" in err and "resuming validator" in err  # stages kept
    assert "codex daemon failed to bind" in err  # final error visible


# --- short resume ids ---


def test_ls_json_short_ids_on_valid_snapshots_only(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _save_snap("0-w2", created_at="100.0")   # → prev after new instance
    _save_snap("0-w2", created_at="200.0")
    (tmp_path / ".hive" / "state" / "resume" / "broken.json").write_text("{nope")
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%9", "liveonly", "worker", "claude")],
        window_list=[{"window": "dev:9", "windowName": "x", "windowId": "@9", "team": "liveonly", "workspace": "/w", "created": "1.0", "pr": ""}],
    )

    result = runner.invoke(cli, ["ls", "--json"])
    rows = {e.get("handle") or e.get("team"): e for e in json.loads(result.stdout)["teams"]}
    assert rows["0-w2"]["shortId"] == "775b"
    assert rows["0-w2.prev"]["shortId"] == resume_store.short_id({"handle": "0-w2.prev", "createdAt": "100.0"})
    assert "shortId" not in rows["broken"]
    assert "shortId" not in rows["liveonly"]


def test_resume_short_id_resolves_to_full_restore(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _dead_setup(monkeypatch, tmp_path)  # handle 0-w2, createdAt 100.0 → sid 0582

    result = runner.invoke(cli, ["resume", "0582"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["resumed"] == "full" and payload["team"] == "0-w2"
    assert len(rec.spawns) == 2  # identical restore path as the full handle
    assert rec.selects == ["dev:7"]


def test_resume_short_id_collision_fails_with_full_handles(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _tmux_state(monkeypatch, pane_list=[], window_list=[])
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    _save_snap("teamA", cwd=good_cwd)
    _save_snap("teamB", cwd=good_cwd)
    sid = resume_store.short_id({"handle": "teamA", "createdAt": "100.0"})
    monkeypatch.setattr("hive.resume.short_id", lambda _s: sid)  # force a collision

    result = runner.invoke(cli, ["resume", sid])
    assert result.exit_code != 0
    assert "hive resume teamA" in result.output and "hive resume teamB" in result.output
    _assert_zero_mutation(rec)


def test_exact_handle_beats_short_id_and_hint_falls_back(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    _tmux_state(monkeypatch, pane_list=[], window_list=[])
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    # a snapshot literally named like the other one's short id
    other_sid = resume_store.short_id({"handle": "victim", "createdAt": "100.0"})
    _save_snap(other_sid, cwd=good_cwd)
    _save_snap("victim", cwd=good_cwd)

    # resolver: the token hits the exact handle, never the victim
    result = runner.invoke(cli, ["resume", other_sid])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["team"] == other_sid

    # hint: victim's row must not advertise a token that resolves elsewhere
    ls = runner.invoke(cli, ["ls", "--plain"])
    victim_line = next(l for l in ls.output.splitlines() if l.strip().startswith("victim"))
    assert "hive resume victim" in victim_line


# --- hive resume: validator without a saved session comes back fresh ---


def _fresh_validator_members(worker_cwd, validator_cwd):
    return [
        {"name": "worker", "cli": "claude", "model": "m1", "sessionId": "sid-w", "cwd": worker_cwd},
        {"name": "validator", "cli": "codex", "model": "m2", "sessionId": "", "cwd": validator_cwd},
    ]


def test_resume_full_restore_spawns_fresh_validator_without_session(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Dead team, worker session saved, validator session empty and its cwd
    deleted: resume succeeds — worker resumes, validator respawns fresh with
    its role bootstrap in the worker's cwd."""
    import hive.cli as cli_mod

    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    gone_cwd = str(tmp_path / "deleted-worktree")  # never created on disk
    _save_snap(
        "0-w2", cwd=good_cwd, workspace=str(tmp_path / "ws"),
        members=_fresh_validator_members(good_cwd, gone_cwd),
    )
    _tmux_state(monkeypatch, pane_list=[], window_list=[])

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)

    # window opens in the worker's cwd; worker spawns first
    assert rec.new_windows[0]["cwd"] == good_cwd
    worker, validator = rec.spawns
    assert worker["name"] == "worker"
    assert worker["session_id"] == "sid-w" and worker["session_mode"] == "resume"
    assert worker["cwd"] == good_cwd and worker["skill"] == "none"
    # fresh validator: role bootstrap prompt, no session resume, worker's cwd,
    # snapshot cli/model
    assert validator["name"] == "validator"
    assert "session_id" not in validator and "session_mode" not in validator
    assert validator["prompt"] == cli_mod._role_bootstrap_prompt("duo-validator")
    assert validator["skill"] == "none"
    assert validator["cwd"] == good_cwd
    assert validator["cli"] == "codex" and validator["model"] == "m2"

    sessions = {m["name"]: m["session"] for m in payload["members"]}
    assert sessions == {"worker": "resumed", "validator": "fresh"}
    # progress must not claim the fresh validator is replaying a session
    assert "spawning fresh validator" in result.stderr
    assert "resuming validator" not in result.stderr
    # transaction tail unchanged: peer, layout, sidecar
    assert rec.peers == [("worker", "validator")]
    assert rec.sidecars and rec.sidecars[0][0] == "0-w2"


def test_resume_live_revive_spawns_fresh_validator_in_live_worker_cwd(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Live worker + missing validator with no saved session: fresh spawn into
    the existing window, following the live worker pane's current cwd."""
    import hive.cli as cli_mod

    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    _save_snap(
        "0-w2", cwd=good_cwd, workspace="/tmp/ws",
        members=_fresh_validator_members(good_cwd, str(tmp_path / "gone")),
    )
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%1", "0-w2", "worker", "claude")],
        window_list=[{
            "window": "dev:3", "windowName": "pair", "windowId": "@3",
            "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0",
        }],
    )

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)

    assert payload["resumed"] == "members"
    assert rec.new_windows == []  # no new window for a live team
    (validator,) = rec.spawns
    assert validator["name"] == "validator"
    assert "session_id" not in validator and "session_mode" not in validator
    assert validator["prompt"] == cli_mod._role_bootstrap_prompt("duo-validator")
    # live worker pane's current cwd (mocked display_value), not the snapshot's
    assert validator["cwd"] == "/live/repo-cwd"
    sessions = {m["name"]: m["session"] for m in payload["members"]}
    assert sessions == {"validator": "fresh"}
    assert "spawning fresh validator" in result.stderr


def test_resume_live_missing_worker_resumes_worker_only(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """Reverse protection: live validator + missing worker (worker session
    saved, validator's empty) resumes the worker only — the live validator is
    never respawned or duplicated."""
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    _save_snap(
        "0-w2", cwd=good_cwd, workspace="/tmp/ws",
        members=_fresh_validator_members(good_cwd, str(tmp_path / "gone")),
    )
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%2", "0-w2", "validator", "codex")],
        window_list=[{
            "window": "dev:3", "windowName": "pair", "windowId": "@3",
            "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0",
        }],
    )

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)

    (worker,) = rec.spawns
    assert worker["name"] == "worker"
    assert worker["session_id"] == "sid-w" and worker["session_mode"] == "resume"
    sessions = {m["name"]: m["session"] for m in payload["members"]}
    assert sessions == {"worker": "resumed"}


def test_resume_live_fresh_validator_rolls_back_pane_on_layout_failure(
    runner, configure_hive_home, monkeypatch, tmp_path
):
    """The fresh-spawn branch keeps the live-revive transaction: a layout
    failure after the fresh validator spawned kills only the new pane and
    leaves the live worker and window untouched."""
    configure_hive_home()
    rec = _resume_mocks(monkeypatch)
    good_cwd = str(tmp_path / "repo")
    (tmp_path / "repo").mkdir()
    _save_snap(
        "0-w2", cwd=good_cwd, workspace="/tmp/ws",
        members=_fresh_validator_members(good_cwd, str(tmp_path / "gone")),
    )
    _tmux_state(
        monkeypatch,
        pane_list=[_pane("%1", "0-w2", "worker", "claude")],
        window_list=[{
            "window": "dev:3", "windowName": "pair", "windowId": "@3",
            "team": "0-w2", "workspace": "/tmp/ws", "created": "100.0",
        }],
    )

    def boom(_target):
        raise RuntimeError("layout boom")

    monkeypatch.setattr("hive.layout.apply_adaptive", boom)

    result = runner.invoke(cli, ["resume", "0-w2"])
    assert result.exit_code != 0
    assert rec.killed_panes == ["%71"]  # only the fresh validator pane
    assert rec.killed_windows == []    # live window survives
    # snapshot is kept for retry
    assert resume_store.load_snapshot("0-w2") is not None
