from types import SimpleNamespace

import pytest

from hive import hived

pytestmark = pytest.mark.unit


# --- grok per-pane leader reap ---------------------------------------------


@pytest.fixture
def reap_env(monkeypatch, tmp_path):
    """Daemon keys on disk; records emit/drop/kill call order."""
    calls: list[tuple] = []
    state = {"keys": [], "pidfiles": {}}

    class _Pool:
        def drop_key(self, key: str) -> None:
            calls.append(("drop", key))

    module = "hive.adapters.grok_leader"
    monkeypatch.setattr(f"{module}.list_daemon_keys", lambda: list(state["keys"]))
    monkeypatch.setattr(
        f"{module}.socket_path_for_key", lambda key: tmp_path / f"{key}.sock"
    )
    monkeypatch.setattr(
        f"{module}.kill_daemon_key", lambda key: calls.append(("kill", key))
    )
    monkeypatch.setattr(f"{module}.pool", lambda: _Pool())
    monkeypatch.setattr(
        "hive.notify_debug.emit",
        lambda workspace, event, **fields: calls.append(("emit", workspace, event, fields)),
    )
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / ".hive"))
    return calls, state, tmp_path


def test_cleanup_skips_live_pane(monkeypatch, reap_env):
    calls, state, _ = reap_env
    state["keys"] = ["p4"]
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: True)

    hived._cleanup_dead_daemons("/tmp/ws")

    assert calls == []


def test_cleanup_reaps_dead_pane_and_logs_before_kill(monkeypatch, reap_env):
    calls, state, _ = reap_env
    state["keys"] = ["p4"]
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: False)

    hived._cleanup_dead_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"key": "p4"}),
        ("drop", "p4"),  # dropped first so a dying grok stdio client
        ("kill", "p4"),  # cannot auto-spawn a replacement leader
    ]


def _write_pidfile(tmp_path, key, age_seconds):
    import os
    import time as _time

    pidfile = tmp_path / f"{key}.pid"
    pidfile.write_text("12345")
    stamp = _time.time() - age_seconds
    os.utime(pidfile, (stamp, stamp))
    return pidfile


def test_cleanup_member_daemon_reaped_when_registry_lists_no_such_member(reap_env):
    calls, state, tmp_path = reap_env
    state["keys"] = ["m-honey.rex"]
    _write_pidfile(tmp_path, "m-honey.rex", age_seconds=999)
    from hive import registry

    assert registry.record_team(
        team="honey", workspace="/ws", created_at="1.0",
        members=[{"name": "other", "cli": "grok"}],
    ) == "written"

    hived._cleanup_dead_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"key": "m-honey.rex"}),
        ("drop", "m-honey.rex"),
        ("kill", "m-honey.rex"),
    ]


def test_cleanup_member_daemon_kept_while_registry_lists_it(reap_env):
    calls, state, tmp_path = reap_env
    state["keys"] = ["m-honey.rex"]
    _write_pidfile(tmp_path, "m-honey.rex", age_seconds=999)
    from hive import registry

    assert registry.record_team(
        team="honey", workspace="/ws", created_at="1.0",
        members=[{"name": "rex", "cli": "grok"}],
    ) == "written"

    hived._cleanup_dead_daemons("/tmp/ws")

    assert calls == []


def test_cleanup_member_daemon_survives_unreadable_registry(reap_env):
    """A corrupt entry is not proof of absence — never reap on a bad read."""
    calls, state, tmp_path = reap_env
    state["keys"] = ["m-honey.rex"]
    _write_pidfile(tmp_path, "m-honey.rex", age_seconds=999)
    from hive import registry

    path = registry.entry_path("honey")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not json")

    hived._cleanup_dead_daemons("/tmp/ws")

    assert calls == []


def test_cleanup_member_daemon_missing_registry_reaps_after_grace(reap_env):
    calls, state, tmp_path = reap_env
    state["keys"] = ["m-honey.rex"]

    # newborn: inside the grace window, spawn registration may be in flight
    _write_pidfile(tmp_path, "m-honey.rex", age_seconds=5)
    hived._cleanup_dead_daemons("/tmp/ws")
    assert calls == []

    # past the grace window with no registry entry: orphan
    _write_pidfile(tmp_path, "m-honey.rex", age_seconds=999)
    hived._cleanup_dead_daemons("/tmp/ws")
    assert ("kill", "m-honey.rex") in calls


# --- codex shared-daemon supervisor -----------------------------------------


def _pane(pane_id, team="", agent="", cli=""):
    return SimpleNamespace(pane_id=pane_id, team=team, agent=agent, cli=cli)


def _member(name, pane_id, cli="codex"):
    return SimpleNamespace(name=name, pane_id=pane_id, cli=cli)


@pytest.fixture
def super_env(monkeypatch):
    """Baseline supervisor world: one live codex member, healthy daemon."""
    state = {
        "panes": [_pane("%1", team="t", agent="val", cli="codex")],
        "recorded": ["%1"],
        "threads": {"%1": "tid-1"},
        "daemon_alive": True,
        "spawn_ok": True,
        "cli_process": {"%1": SimpleNamespace(name="codex")},
        "pane_command": {"%1": "zsh"},
        "calls": [],
    }
    calls = state["calls"]
    cas = "hive.adapters.codex_app_server"
    monkeypatch.setattr("hive.tmux.list_panes_all", lambda: state["panes"])
    monkeypatch.setattr(f"{cas}.list_recorded_panes", lambda: list(state["recorded"]))
    monkeypatch.setattr(
        f"{cas}.clear_pane_thread", lambda pane: calls.append(("clear", pane))
    )
    monkeypatch.setattr(
        f"{cas}.thread_id_for_pane", lambda pane: state["threads"].get(pane)
    )
    monkeypatch.setattr(f"{cas}.daemon_alive", lambda: state["daemon_alive"])
    monkeypatch.setattr(f"{cas}.drop_client", lambda: calls.append(("drop_client",)))
    monkeypatch.setattr(
        f"{cas}.spawn_daemon",
        lambda **_kw: calls.append(("spawn",)) or state["spawn_ok"],
    )
    monkeypatch.setattr(
        "hive.team.Team.load",
        classmethod(lambda _cls, _name, **_kw: SimpleNamespace(agents={
            p.agent: _member(p.agent, p.pane_id, cli=p.cli)
            for p in state["panes"] if p.agent
        })),
    )
    monkeypatch.setattr(
        "hive.hived.detect_cli_process_for_pane",
        lambda pane: state["cli_process"].get(pane),
    )
    monkeypatch.setattr(
        "hive.tmux.display_value",
        lambda pane, _fmt: state["pane_command"].get(pane, ""),
    )
    monkeypatch.setattr(
        "hive.tmux.send_keys",
        lambda pane, text, enter=True: calls.append(("send", pane, text)),
    )
    monkeypatch.setattr(
        "hive.notify_debug.emit",
        lambda workspace, event, **fields: calls.append(("emit", event, fields)),
    )
    monkeypatch.setattr(hived, "_CODEX_REATTACH_AT", {})
    return state


def test_supervisor_healthy_world_does_nothing(super_env):
    hived._codex_supervisor_tick("/tmp/ws", "t")
    assert super_env["calls"] == []


def test_supervisor_prunes_records_of_dead_panes(super_env):
    super_env["recorded"] = ["%1", "%dead"]
    hived._codex_supervisor_tick("/tmp/ws", "t")
    assert ("clear", "%dead") in super_env["calls"]
    assert ("clear", "%1") not in super_env["calls"]


def test_supervisor_leaves_daemon_alone_without_codex_members(super_env):
    # Machine-level shared daemon: a team with no live codex member must not
    # respawn (or otherwise touch) it — other teams may be using it.
    super_env["panes"] = [_pane("%9", team="t", agent="w", cli="claude")]
    super_env["recorded"] = []
    super_env["daemon_alive"] = False
    hived._codex_supervisor_tick("/tmp/ws", "t")
    assert super_env["calls"] == []


def test_supervisor_respawns_dead_daemon_with_live_member(super_env):
    super_env["daemon_alive"] = False
    hived._codex_supervisor_tick("/tmp/ws", "t")
    calls = super_env["calls"]
    assert ("drop_client",) in calls  # stale client must reconnect post-respawn
    assert ("spawn",) in calls
    assert ("emit", "codex.daemon.respawn", {"ok": True}) in calls


def test_supervisor_reattaches_retained_shell(super_env):
    super_env["cli_process"] = {}  # CLI exited; pane keeps its shell
    hived._codex_supervisor_tick("/tmp/ws", "t")
    calls = super_env["calls"]
    assert ("send", "%1", "hive codex resume tid-1") in calls
    assert ("emit", "codex.member.reattach",
            {"pane": "%1", "agent": "val", "thread": "tid-1"}) in calls


def test_supervisor_reattach_respects_cooldown(super_env):
    super_env["cli_process"] = {}
    hived._codex_supervisor_tick("/tmp/ws", "t")
    hived._codex_supervisor_tick("/tmp/ws", "t")
    sends = [c for c in super_env["calls"] if c[0] == "send"]
    assert len(sends) == 1  # one attempt per cooldown window


def test_supervisor_never_types_over_a_live_cli(super_env):
    hived._codex_supervisor_tick("/tmp/ws", "t")
    assert [c for c in super_env["calls"] if c[0] == "send"] == []


def test_supervisor_never_types_into_a_non_shell(super_env):
    super_env["cli_process"] = {}
    super_env["pane_command"] = {"%1": "vim"}
    hived._codex_supervisor_tick("/tmp/ws", "t")
    assert [c for c in super_env["calls"] if c[0] == "send"] == []


def test_supervisor_skips_member_without_record(super_env):
    super_env["cli_process"] = {}
    super_env["threads"] = {}
    hived._codex_supervisor_tick("/tmp/ws", "t")
    assert [c for c in super_env["calls"] if c[0] == "send"] == []
