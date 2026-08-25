from types import SimpleNamespace

import pytest

from hive import sidecar

pytestmark = pytest.mark.unit


# --- grok per-pane leader reap ---------------------------------------------


@pytest.fixture
def reap_env(monkeypatch, tmp_path):
    """One grok leader pane on disk; records emit/drop/kill call order."""
    calls: list[tuple] = []
    pidfile = tmp_path / "p4.pid"

    class _Pool:
        def drop(self, pane: str) -> None:
            calls.append(("drop", pane))

    module = "hive.adapters.grok_leader"
    monkeypatch.setattr(f"{module}.list_daemon_panes", lambda: ["%4"])
    monkeypatch.setattr(f"{module}.pane_pidfile_path", lambda pane: pidfile)
    monkeypatch.setattr(
        f"{module}.kill_pane_daemon", lambda pane: calls.append(("kill", pane))
    )
    monkeypatch.setattr(f"{module}.pool", lambda: _Pool())
    monkeypatch.setattr(
        "hive.notify_debug.emit",
        lambda workspace, event, **fields: calls.append(("emit", workspace, event, fields)),
    )
    return calls, pidfile


def test_cleanup_skips_live_pane(monkeypatch, reap_env):
    calls, _ = reap_env
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: True)

    sidecar._cleanup_dead_daemons("/tmp/ws")

    assert calls == []


def test_cleanup_reaps_dead_pane_and_logs_before_kill(monkeypatch, reap_env):
    calls, pidfile = reap_env
    pidfile.write_text("12345")
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: False)

    sidecar._cleanup_dead_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"pane": "%4", "pid": 12345}),
        ("drop", "%4"),  # dropped first so a dying grok stdio client
        ("kill", "%4"),  # cannot auto-spawn a replacement leader
    ]


def test_cleanup_logs_reap_without_readable_pidfile(monkeypatch, reap_env):
    calls, _ = reap_env
    monkeypatch.setattr("hive.tmux.is_pane_alive", lambda pane: False)

    sidecar._cleanup_dead_daemons("/tmp/ws")

    assert calls == [
        ("emit", "/tmp/ws", "daemon.reap", {"pane": "%4", "pid": None}),
        ("drop", "%4"),
        ("kill", "%4"),
    ]


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
        "hive.sidecar.detect_cli_process_for_pane",
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
    monkeypatch.setattr(sidecar, "_CODEX_REATTACH_AT", {})
    return state


def test_supervisor_healthy_world_does_nothing(super_env):
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    assert super_env["calls"] == []


def test_supervisor_prunes_records_of_dead_panes(super_env):
    super_env["recorded"] = ["%1", "%dead"]
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    assert ("clear", "%dead") in super_env["calls"]
    assert ("clear", "%1") not in super_env["calls"]


def test_supervisor_leaves_daemon_alone_without_codex_members(super_env):
    # Machine-level shared daemon: a team with no live codex member must not
    # respawn (or otherwise touch) it — other teams may be using it.
    super_env["panes"] = [_pane("%9", team="t", agent="w", cli="claude")]
    super_env["recorded"] = []
    super_env["daemon_alive"] = False
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    assert super_env["calls"] == []


def test_supervisor_respawns_dead_daemon_with_live_member(super_env):
    super_env["daemon_alive"] = False
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    calls = super_env["calls"]
    assert ("drop_client",) in calls  # stale client must reconnect post-respawn
    assert ("spawn",) in calls
    assert ("emit", "codex.daemon.respawn", {"ok": True}) in calls


def test_supervisor_reattaches_retained_shell(super_env):
    super_env["cli_process"] = {}  # CLI exited; pane keeps its shell
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    calls = super_env["calls"]
    assert ("send", "%1", "hive codex resume tid-1") in calls
    assert ("emit", "codex.member.reattach",
            {"pane": "%1", "agent": "val", "thread": "tid-1"}) in calls


def test_supervisor_reattach_respects_cooldown(super_env):
    super_env["cli_process"] = {}
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    sends = [c for c in super_env["calls"] if c[0] == "send"]
    assert len(sends) == 1  # one attempt per cooldown window


def test_supervisor_never_types_over_a_live_cli(super_env):
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    assert [c for c in super_env["calls"] if c[0] == "send"] == []


def test_supervisor_never_types_into_a_non_shell(super_env):
    super_env["cli_process"] = {}
    super_env["pane_command"] = {"%1": "vim"}
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    assert [c for c in super_env["calls"] if c[0] == "send"] == []


def test_supervisor_skips_member_without_record(super_env):
    super_env["cli_process"] = {}
    super_env["threads"] = {}
    sidecar._codex_supervisor_tick("/tmp/ws", "t")
    assert [c for c in super_env["calls"] if c[0] == "send"] == []
