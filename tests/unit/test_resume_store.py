import json

import pytest

from hive import resume


@pytest.fixture
def store(monkeypatch, tmp_path):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / ".hive"))
    return tmp_path / ".hive" / "state" / "resume"


def _snap(handle="0-w2", created_at="100.0", session="sid-worker"):
    return resume.build_snapshot(
        handle=handle,
        team=handle,
        group="duo",
        window_name="hive",
        workspace="/tmp/hive-0-w2",
        repo_cwd="/repo",
        branch="main",
        created_at=created_at,
        members=[
            {"name": "worker", "cli": "claude", "model": "m1", "sessionId": session, "cwd": "/repo"},
            {"name": "validator", "cli": "codex", "model": "m2", "sessionId": "sid-val", "cwd": "/repo"},
        ],
    )


def test_snapshot_round_trip_and_atomic_file(store):
    snap = _snap()
    assert resume.save_snapshot(snap, now="2026-07-10T00:00:00Z") == "written"

    loaded = resume.load_snapshot("0-w2")
    assert loaded is not None
    assert loaded["savedAt"] == "2026-07-10T00:00:00Z"
    assert {m["name"] for m in loaded["members"]} == {"worker", "validator"}
    assert not list(store.glob(".snap.*"))  # no temp files left behind


def test_corrupt_and_unknown_schema_are_tolerated(store):
    store.mkdir(parents=True)
    (store / "bad.json").write_text("{not json")
    (store / "old.json").write_text(json.dumps({"schema": 99, "handle": "old", "team": "old"}))

    assert resume.load_snapshot("bad") is None
    assert resume.load_snapshot("old") is None
    listed = {s["handle"]: s for s in resume.list_snapshots()}
    assert listed["bad"].get("corrupt") is True
    assert listed["old"].get("corrupt") is True


def test_unsafe_handles_cannot_escape_store(store):
    for handle in ("../evil", "a/b", "", ".hidden", "a..b"):
        assert resume.snapshot_path(handle) is None
        snap = _snap()
        snap["handle"] = handle
        assert resume.save_snapshot(snap, now="t") == "rejected"


def test_new_instance_archives_one_predecessor(store):
    assert resume.save_snapshot(_snap(created_at="100.0"), now="t1") == "written"
    assert resume.save_snapshot(_snap(created_at="200.0", session="sid-2"), now="t2") == "written"

    prev = resume.load_snapshot("0-w2.prev")
    assert prev is not None and prev["createdAt"] == "100.0"
    cur = resume.load_snapshot("0-w2")
    assert cur is not None and cur["createdAt"] == "200.0"

    # A third instance replaces the single predecessor slot, never grows it.
    assert resume.save_snapshot(_snap(created_at="300.0", session="sid-3"), now="t3") == "written"
    assert resume.load_snapshot("0-w2.prev")["createdAt"] == "200.0"
    assert len(list(store.glob("0-w2*.json"))) == 2


def test_resume_continuation_does_not_archive(store):
    assert resume.save_snapshot(_snap(created_at="100.0"), now="t1") == "written"
    cont = _snap(created_at="400.0", session="sid-resumed")
    assert resume.save_snapshot(cont, now="t2", archive_on_new_instance=False) == "written"

    assert resume.load_snapshot("0-w2")["createdAt"] == "400.0"
    assert resume.load_snapshot("0-w2.prev") is None


def test_unchanged_payload_never_rewrites_or_bumps_saved_at(store):
    snap = _snap()
    assert resume.save_snapshot(snap, now="t1") == "written"
    path = store / "0-w2.json"
    before = path.read_text()

    assert resume.save_snapshot(_snap(), now="t2-later") == "unchanged"
    assert path.read_text() == before  # savedAt still t1, no rewrite

    changed = _snap(session="sid-new")
    assert resume.save_snapshot(changed, now="t3") == "written"
    assert resume.load_snapshot("0-w2")["savedAt"] == "t3"


def test_merge_members_keeps_dead_member_and_updates_observed(store):
    existing = _snap()["members"]
    # validator pane died: only the worker is observed, with a fresher session.
    merged = resume.merge_members(existing, [{"name": "worker", "sessionId": "sid-worker-2"}])

    by_name = {m["name"]: m for m in merged}
    assert by_name["worker"]["sessionId"] == "sid-worker-2"
    assert by_name["worker"]["cli"] == "claude"  # empty observation didn't erase
    assert by_name["validator"]["sessionId"] == "sid-val"  # dead member survives

    # validator comes back: only its fields update.
    merged2 = resume.merge_members(merged, [{"name": "validator", "sessionId": "sid-val-2", "cli": "codex"}])
    by_name2 = {m["name"]: m for m in merged2}
    assert by_name2["validator"]["sessionId"] == "sid-val-2"
    assert by_name2["worker"]["sessionId"] == "sid-worker-2"


# --- sidecar writer: roster-merged persistence (VAL A2-A3) ---


def _fake_team(agents, groups=None):
    from types import SimpleNamespace

    return SimpleNamespace(
        name="0-w2",
        tmux_window="dev:0",
        created_at=123.0,
        agents=agents,
        member_groups=groups or {name: "duo" for name in agents},
    )


def _fake_agent(pane, cli):
    from types import SimpleNamespace

    return SimpleNamespace(pane_id=pane, cli=cli, model="", session_id="", cwd="/repo")


def _writer_mocks(monkeypatch, team, sessions):
    from hive import sidecar

    monkeypatch.setattr("hive.team.Team.load", staticmethod(lambda name, prefer_pane="": team))
    monkeypatch.setattr("hive.sidecar._fresh_snapshot_session_id", lambda pane: sessions.get(pane, ""))
    monkeypatch.setattr(
        "hive.agent_cli.resolve_model_for_pane",
        lambda pane, cli_name="", current_model="": f"m-{cli_name}",
    )
    monkeypatch.setattr("hive.tmux.list_window_names", lambda: [("dev:0", "hive")])
    monkeypatch.setattr("hive.resume.git_branch", lambda cwd: "main")
    return sidecar


def test_writer_persists_full_roster_then_keeps_dead_member(store, monkeypatch):
    worker = _fake_agent("%1", "claude")
    validator = _fake_agent("%2", "codex")
    sidecar = _writer_mocks(
        monkeypatch,
        _fake_team({"worker": worker, "validator": validator}),
        {"%1": "sid-w", "%2": "sid-v"},
    )

    sidecar._write_resume_snapshot("/ws", "0-w2")
    snap = resume.load_snapshot("0-w2")
    by_name = {m["name"]: m for m in snap["members"]}
    assert by_name["worker"]["sessionId"] == "sid-w"
    assert by_name["validator"]["sessionId"] == "sid-v"
    assert by_name["validator"]["model"] == "m-codex"
    assert snap["windowName"] == "hive" and snap["branch"] == "main"

    # validator pane dies: only the worker is observed now, with a rotated session.
    sidecar2 = _writer_mocks(
        monkeypatch, _fake_team({"worker": worker}), {"%1": "sid-w2"}
    )
    sidecar2._write_resume_snapshot("/ws", "0-w2")
    snap2 = resume.load_snapshot("0-w2")
    by_name2 = {m["name"]: m for m in snap2["members"]}
    assert by_name2["validator"]["sessionId"] == "sid-v"  # dead member survives
    assert by_name2["worker"]["sessionId"] == "sid-w2"


def test_writer_skips_non_duo_and_unloadable_teams(store, monkeypatch):
    worker = _fake_agent("%1", "claude")
    sidecar = _writer_mocks(
        monkeypatch,
        _fake_team({"worker": worker}, groups={"worker": ""}),
        {"%1": "sid-w"},
    )
    sidecar._write_resume_snapshot("/ws", "0-w2")
    assert resume.load_snapshot("0-w2") is None

    def _boom(name, prefer_pane=""):
        raise FileNotFoundError(name)

    monkeypatch.setattr("hive.team.Team.load", staticmethod(_boom))
    sidecar._write_resume_snapshot("/ws", "0-w2")
    assert resume.load_snapshot("0-w2") is None


def test_writer_never_leaks_sessions_across_instances(store, monkeypatch):
    """A same-handle NEW team must not inherit the previous instance's sessions."""
    old_worker = _fake_agent("%1", "claude")
    old_val = _fake_agent("%2", "codex")
    sidecar = _writer_mocks(
        monkeypatch,
        _fake_team({"worker": old_worker, "validator": old_val}),
        {"%1": "OLD-W", "%2": "OLD-V"},
    )
    sidecar._write_resume_snapshot("/ws", "0-w2")
    assert resume.load_snapshot("0-w2")["createdAt"] == "123.0"

    # new instance (createdAt differs), only a worker observed, session unresolved
    new_team = _fake_team({"worker": _fake_agent("%9", "claude")})
    new_team.created_at = 200.0
    sidecar2 = _writer_mocks(monkeypatch, new_team, {})
    sidecar2._write_resume_snapshot("/ws", "0-w2")

    cur = resume.load_snapshot("0-w2")
    assert cur["createdAt"] == "200.0"
    names = {m["name"]: m for m in cur["members"]}
    assert set(names) == {"worker"}  # old validator did not cross instances
    assert names["worker"]["sessionId"] == ""  # OLD-W did not leak in
    prev = resume.load_snapshot("0-w2.prev")
    assert prev is not None and prev["createdAt"] == "123.0"
    assert {m["sessionId"] for m in prev["members"]} == {"OLD-W", "OLD-V"}


def test_prev_namespace_is_reserved_for_archives(store, monkeypatch):
    """A real team named foo.prev must never collide with foo's archive slot."""
    # the store refuses .prev as a primary handle — no silent overwrite target
    impostor = _snap(handle="foo.prev", created_at="50.0")
    impostor["team"] = "foo.prev"
    assert resume.save_snapshot(impostor, now="t0") == "rejected"
    assert resume.load_snapshot("foo.prev") is None

    # foo's own archive flow now owns that name unambiguously
    assert resume.save_snapshot(_snap(handle="foo", created_at="100.0"), now="t1") == "written"
    assert resume.save_snapshot(_snap(handle="foo", created_at="200.0"), now="t2") == "written"
    prev = resume.load_snapshot("foo.prev")
    assert prev is not None and prev["createdAt"] == "100.0" and prev["team"] == "foo"

    # and team creation rejects the reserved suffix before any tmux mutation
    from hive.team import Team

    calls: list[str] = []
    monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.team.tmux.get_window_option", lambda *a: calls.append("read") or None)
    monkeypatch.setattr("hive.team.tmux.set_window_option", lambda *a: calls.append("write"))
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *a, **k: calls.append("write"))
    with pytest.raises(ValueError, match="reserved"):
        Team.create_for_window("foo.prev", window_target="dev:0")
    assert "write" not in calls
