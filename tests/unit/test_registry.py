"""The team registry: write-lane authority, instance guards, hived backfill."""
import json

import pytest

from hive import registry

pytestmark = pytest.mark.unit


@pytest.fixture
def store(monkeypatch, tmp_path):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / ".hive"))
    return tmp_path / ".hive" / "state" / "teams"


def test_record_team_round_trip_and_atomic_file(store):
    assert registry.record_team(
        team="honey", workspace="/ws", created_at="100.0",
        members=[{"name": "worker", "cli": "claude", "sessionId": "sid-w"}],
        display="@3",
    ) == "written"

    entry = registry.load("honey")
    assert entry is not None
    assert entry["workspace"] == "/ws"
    assert entry["display"] == "@3"
    assert {m["name"] for m in entry["members"]} == {"worker"}
    assert not list(store.glob(".reg.*"))  # no temp files left behind


def test_corrupt_entries_are_tolerated_and_marked(store):
    store.mkdir(parents=True)
    (store / "bad.json").write_text("{not json")

    assert registry.load("bad") is None
    listed = {e["team"]: e for e in registry.list_entries()}
    assert listed["bad"].get("corrupt") is True


def test_unsafe_names_cannot_escape_store(store):
    for name in ("../evil", "a/b", "", ".hidden", "a..b"):
        assert registry.entry_path(name) is None
        assert registry.record_team(team=name, workspace="", created_at="1.0") == "rejected"


def test_record_team_overwrites_a_recycled_names_predecessor(store):
    assert registry.record_team(
        team="honey", workspace="/old", created_at="100.0",
        members=[{"name": "worker", "sessionId": "OLD-SID"}],
    ) == "written"
    assert registry.record_team(
        team="honey", workspace="/new", created_at="200.0",
    ) == "written"

    entry = registry.load("honey")
    assert entry["createdAt"] == "200.0"
    assert entry["members"] == []  # nothing inherited from the predecessor


def test_record_and_remove_member_guard_the_instance(store):
    assert registry.record_team(
        team="honey", workspace="/ws", created_at="123.0",
        members=[{"name": "worker", "cli": "claude"}],
    ) == "written"
    row = {"name": "validator", "cli": "codex", "sessionId": "sid-v"}
    assert registry.record_member("honey", row, created_at="999.0") == "missing"
    assert registry.record_member("honey", row, created_at="123.0") == "written"
    assert {m["name"] for m in registry.load("honey")["members"]} == {"worker", "validator"}
    assert registry.remove_member("honey", "validator", created_at="999.0") == "missing"
    assert registry.remove_member("honey", "validator", created_at="123.0") == "written"
    assert {m["name"] for m in registry.load("honey")["members"]} == {"worker"}


def test_delete_team_removes_the_entry(store):
    assert registry.record_team(team="honey", workspace="/ws", created_at="1.0") == "written"
    registry.delete_team("honey")
    assert registry.load("honey") is None
    assert registry.entry_path("honey") is not None
    assert not registry.entry_path("honey").is_file()


def test_backfill_members_keeps_dead_updates_observed_never_adds(store):
    existing = [
        {"name": "worker", "cli": "claude", "model": "m1", "sessionId": "sid-w", "cwd": "/repo"},
        {"name": "validator", "cli": "codex", "model": "m2", "sessionId": "sid-val", "cwd": "/repo"},
    ]
    merged = registry.backfill_members(existing, [{"name": "worker", "sessionId": "sid-w2"}])
    by_name = {m["name"]: m for m in merged}
    assert by_name["worker"]["sessionId"] == "sid-w2"
    assert by_name["worker"]["cli"] == "claude"  # empty observation didn't erase
    assert by_name["validator"]["sessionId"] == "sid-val"  # dead member survives

    # membership belongs to the CLI writers: an observed stranger (e.g. a
    # kill racing this observation) is never added back to the roster.
    merged2 = registry.backfill_members(merged, [{"name": "ghost", "cli": "claude"}])
    assert {m["name"] for m in merged2} == {"worker", "validator"}


def test_backfill_refuses_missing_and_foreign_instance(store):
    observed = [{"name": "worker", "cli": "claude", "sessionId": "sid-w"}]
    assert registry.backfill("honey", observed, created_at="123.0") == "missing"

    assert registry.record_team(
        team="honey", workspace="/ws", created_at="123.0",
        members=[{"name": "worker"}],
    ) == "written"
    assert registry.backfill("honey", observed, created_at="999.0") == "missing"
    assert registry.load("honey")["members"][0]["sessionId"] == ""

    assert registry.backfill("honey", observed, created_at="123.0") == "written"
    assert registry.load("honey")["members"][0]["sessionId"] == "sid-w"
    assert registry.backfill("honey", observed, created_at="123.0") == "unchanged"


def test_backfill_observation_never_resurrects_a_killed_member(store):
    """The kill-vs-backfill race, closed by construction."""
    assert registry.record_team(
        team="honey", workspace="/ws", created_at="123.0",
        members=[{"name": "worker"}, {"name": "victim"}],
    ) == "written"
    observed = [
        {"name": "worker", "cli": "claude", "sessionId": "sid-w"},
        {"name": "victim", "cli": "codex", "sessionId": "sid-v"},
    ]
    # hive kill removed the member between the observation and the write
    assert registry.remove_member("honey", "victim", created_at="123.0") == "written"

    assert registry.backfill("honey", observed, created_at="123.0") == "written"

    assert {m["name"] for m in registry.load("honey")["members"]} == {"worker"}


# --- hived writer over the registry ---------------------------------------


def _fake_team(agents):
    from types import SimpleNamespace

    return SimpleNamespace(
        name="honey",
        tmux_window="dev:0",
        tmux_window_id="@0",
        created_at=123.0,
        agents=agents,
    )


def _fake_agent(pane, cli):
    from types import SimpleNamespace

    return SimpleNamespace(pane_id=pane, cli=cli, model="", session_id="", cwd="/repo")


def _writer_mocks(monkeypatch, team, sessions):
    from hive import hived

    monkeypatch.setattr("hive.team.Team.load", staticmethod(lambda name, prefer_pane="": team))
    monkeypatch.setattr("hive.hived._fresh_snapshot_session_id", lambda pane: sessions.get(pane, ""))
    monkeypatch.setattr(
        "hive.agent_cli.resolve_model_for_pane",
        lambda pane, cli_name="", current_model="": f"m-{cli_name}",
    )
    return hived


def test_writer_backfills_roster_and_display(store, monkeypatch):
    assert registry.record_team(
        team="honey", workspace="/ws", created_at="123.0",
        members=[{"name": "worker"}, {"name": "validator"}],
    ) == "written"
    hived = _writer_mocks(
        monkeypatch,
        _fake_team({"worker": _fake_agent("%1", "claude"), "validator": _fake_agent("%2", "codex")}),
        {"%1": "sid-w", "%2": "sid-v"},
    )

    hived._write_registry_backfill("/ws", "honey")

    entry = registry.load("honey")
    by_name = {m["name"]: m for m in entry["members"]}
    assert by_name["worker"]["sessionId"] == "sid-w"
    assert by_name["validator"]["sessionId"] == "sid-v"
    assert by_name["validator"]["model"] == "m-codex"
    assert entry["display"] == "@0"

    # validator pane dies: only the worker observed, session rotated
    hived2 = _writer_mocks(monkeypatch, _fake_team({"worker": _fake_agent("%1", "claude")}), {"%1": "sid-w2"})
    hived2._write_registry_backfill("/ws", "honey")
    by_name2 = {m["name"]: m for m in registry.load("honey")["members"]}
    assert by_name2["validator"]["sessionId"] == "sid-v"  # dead member survives
    assert by_name2["worker"]["sessionId"] == "sid-w2"


def test_writer_without_registry_entry_writes_nothing(store, monkeypatch):
    """Observation never creates a roster: membership belongs to the CLI."""
    hived = _writer_mocks(
        monkeypatch, _fake_team({"worker": _fake_agent("%1", "claude")}), {"%1": "sid-w"}
    )
    hived._write_registry_backfill("/ws", "honey")
    assert registry.load("honey") is None
