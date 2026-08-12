"""Desktop-led duo formation: `hive duo init --channel` outside tmux.

The external Claude session (Claude Code desktop) becomes the worker; its tmux
presence is an anchor pane whose channel socket/marker are symlinks to the
session's ``hive-client-<pid>.sock``. These tests are hermetic: tmux is the
conftest fake, the validator spawn is stubbed, and the client socket is a
plain file (``link_client_socket`` only checks existence + marker version —
real listening-socket delivery is covered in tests/unit/test_claude_channel.py).
"""
import json
import os
import socket
import tempfile
from pathlib import Path
from types import SimpleNamespace

import pytest

from hive.cli import cli
from hive import context as hive_context
from hive.adapters import claude_channel


def _client_socket_files(hive_home: Path, name: str = "hive-client-777") -> Path:
    channel = hive_home / "channel"
    channel.mkdir(parents=True, exist_ok=True)
    sock = channel / f"{name}.sock"
    sock.touch()
    (channel / f"{name}.ready").write_text("2")
    return sock


@pytest.fixture
def ccd_tmux(monkeypatch):
    """Stub the desktop-led path's tmux mutations.

    Returned as a callable to invoke AFTER ``configure_hive_home(...)``:
    ``hive.cli.tmux`` and ``hive.team.tmux`` are the same module object, so
    these patches must land last to win over the conftest defaults
    (``has_session`` → True in particular).
    """

    def _apply() -> dict[str, object]:
        calls = _patch(monkeypatch)
        return calls

    return _apply


def _patch(monkeypatch) -> dict[str, object]:
    calls: dict[str, object] = {"pane_options": [], "killed": []}
    monkeypatch.setattr("hive.cli.tmux.has_session", lambda _name: False)
    monkeypatch.setattr(
        "hive.cli.tmux.new_session", lambda name: calls.__setitem__("new_session", name) or "%49"
    )
    monkeypatch.setattr(
        "hive.cli.tmux.new_window",
        lambda session, **kw: calls.__setitem__("new_window", (session, kw)) or ("hive-ccd:1", "%50"),
    )
    monkeypatch.setattr("hive.cli.tmux.configure_hive_window", lambda _t: None)
    monkeypatch.setattr("hive.cli.tmux.set_pane_title", lambda *_a: None)
    monkeypatch.setattr(
        "hive.cli.tmux.set_pane_option",
        lambda pane, key, value: calls["pane_options"].append((pane, key, value)),
    )
    monkeypatch.setattr("hive.cli.tmux.kill_window", lambda target: calls["killed"].append(target))
    monkeypatch.setattr("hive.cli.tmux.zoom_pane", lambda pane: calls.__setitem__("zoomed", pane))
    monkeypatch.setattr("hive.cli.tmux.select_window", lambda w: calls.__setitem__("selected", w))
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda _w: None)
    monkeypatch.setattr("hive.sidecar.stop_sidecar", lambda _ws: None)
    monkeypatch.setattr("hive.cli.bus.reset_workspace", lambda _ws: None)
    monkeypatch.setattr(
        "hive.cli._spawn_duo_validator",
        lambda t, **kw: calls.__setitem__("validator_kw", kw) or SimpleNamespace(pane_id="%51"),
    )
    return calls


def test_duo_init_outside_tmux_forms_desktop_led_duo(
    runner, configure_hive_home, ccd_tmux, monkeypatch
):
    hive_home = configure_hive_home(tmux_inside=False)
    calls = ccd_tmux()
    sock = _client_socket_files(hive_home)

    result = runner.invoke(cli, ["duo", "init", "--channel", str(sock)])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["worker"] == {
        "pane": "%50",
        "name": "worker",
        "cli": "claude",
        "remote": "channel",
    }
    assert payload["validator"]["pane"] == "%51"
    assert payload["watch"] == "tmux attach -t hive-ccd"
    assert calls["new_session"] == "hive-ccd"
    assert calls["killed"] == []

    # Anchor pane carries member tags plus the remote marker.
    opts = {(k, v) for _p, k, v in calls["pane_options"]}
    assert ("hive-agent", "worker") in opts
    assert ("hive-cli", "claude") in opts
    assert ("hive-remote", "channel") in opts

    # Channel socket + marker are symlinks to the client's files.
    assert os.readlink(claude_channel.channel_socket_path("%50")) == str(sock)
    assert claude_channel.marker_version("%50") == "2"

    # The default context is the external session's outbound identity.
    ctx = hive_context.load_current_context()
    assert ctx["team"] == payload["team"]
    assert ctx["agent"] == "worker"

    # Validator spawns beside the anchor with the outside-tmux opt-in, and the
    # anti-family choice is derived from the anthropic *family*, not the CLI
    # name (a claude-led desktop duo gets a codex validator).
    assert calls["validator_kw"]["worker_pane"] == "%50"
    assert calls["validator_kw"]["allow_outside_tmux"] is True
    assert calls["validator_kw"]["cli"] == "codex"

    # Viewport: the validator fills the window (anchor is plumbing, not a
    # view) and an attach lands on the duo window, not the empty window 0.
    assert calls["zoomed"] == "%51"
    assert calls["selected"] == "hive-ccd:1"


def test_duo_init_outside_tmux_dead_channel_socket_undoes_window(
    runner, configure_hive_home, ccd_tmux
):
    hive_home = configure_hive_home(tmux_inside=False)
    calls = ccd_tmux()
    missing = hive_home / "channel" / "hive-client-404.sock"

    result = runner.invoke(cli, ["duo", "init", "--channel", str(missing)])

    assert result.exit_code != 0
    assert "does not exist" in result.output
    assert calls["killed"] == ["hive-ccd:1"]  # no half-built window left
    # And no dangling identity: a failed form must not poison the saved
    # context (it would break `hive collect` and the next init's idempotency).
    assert hive_context.load_current_context().get("team", "") == ""


def test_send_outside_tmux_resolves_saved_context(runner, configure_hive_home, monkeypatch):
    """The root gate + team resolution fall back to the saved default context,
    so a desktop-led worker can `hive send` without being inside tmux."""
    configure_hive_home(tmux_inside=False)
    hive_context.save_current_context(team="hive-ccd-w1", workspace="/tmp/ws", agent="worker")
    seen: dict[str, object] = {}

    def _fake_resolve(team, required=True):
        seen["team"] = team
        raise SystemExit(3)  # stop before any tmux resolution

    monkeypatch.setattr("hive.cli._resolve_scoped_team", _fake_resolve)

    result = runner.invoke(cli, ["send", "validator", "hello"])

    # The command got past the root tmux gate and asked for the context team.
    assert result.exit_code == 3
    assert seen["team"] is None or seen["team"] == "hive-ccd-w1"


def test_duo_init_outside_tmux_discovers_single_live_socket(
    runner, configure_hive_home, ccd_tmux, monkeypatch
):
    """With no --channel, a single live (connect-probed) client socket is used;
    corpse socket files without a listener are ignored."""
    configure_hive_home(tmux_inside=False)
    ccd_tmux()
    short_home = Path(tempfile.mkdtemp(prefix="hh", dir="/tmp"))
    monkeypatch.setenv("HIVE_HOME", str(short_home))
    channel = short_home / "channel"
    channel.mkdir(parents=True)
    (channel / "hive-client-1.sock").touch()  # corpse: file, no listener
    live = channel / "hive-client-2.sock"
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(live))
    srv.listen(1)
    (channel / "hive-client-2.ready").write_text("2")
    try:
        result = runner.invoke(cli, ["duo", "init"])
        assert result.exit_code == 0, result.output
        assert os.readlink(claude_channel.channel_socket_path("%50")) == str(live)
    finally:
        srv.close()
        import shutil

        shutil.rmtree(short_home, ignore_errors=True)


def test_existing_ccd_duo_relinks_stale_socket(configure_hive_home, monkeypatch, tmp_path):
    """A desktop session restart changes its pid-keyed socket; re-running init
    heals the anchor's symlink and keeps the team instead of forming a new one."""
    hive_home = configure_hive_home(tmux_inside=False)
    hive_context.save_current_context(team="hive-ccd-w1", workspace="/tmp/ws", agent="worker")
    worker = SimpleNamespace(pane_id="%50", cli="claude")
    team = SimpleNamespace(
        name="hive-ccd-w1", tmux_window="hive-ccd:1", agents={"worker": worker}
    )
    monkeypatch.setattr("hive.cli.Team", SimpleNamespace(load=lambda name, **kw: team))
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda _p, k: "channel" if k == "hive-remote" else None,
    )
    channel = hive_home / "channel"
    channel.mkdir(parents=True, exist_ok=True)
    stale = channel / "hive-client-1.sock"
    stale.touch()
    fresh = channel / "hive-client-2.sock"
    fresh.touch()
    (channel / "hive-client-2.ready").write_text("2")
    link = claude_channel.channel_socket_path("%50")
    os.symlink(stale, link)

    from hive.cli import _existing_ccd_duo

    res = _existing_ccd_duo(str(fresh))

    assert res is not None and res.get("relinked") is True
    assert os.readlink(link) == str(fresh)
    assert claude_channel.marker_version("%50") == "2"


def test_existing_ccd_duo_clears_stale_session_claim(
    configure_hive_home, monkeypatch, tmp_path
):
    """Re-running init from a new desktop session moves the inbox with the
    socket: a dead session's saved claim would otherwise bounce every
    `hive collect --session` as notMine and strand inbound mail."""
    hive_home = configure_hive_home(tmux_inside=False)
    hive_context.save_current_context(
        team="hive-ccd-w1",
        workspace="/tmp/ws",
        agent="worker",
        session="49174206-dead-dead-dead-000000000000",
    )
    worker = SimpleNamespace(pane_id="%50", cli="claude")
    team = SimpleNamespace(
        name="hive-ccd-w1", tmux_window="hive-ccd:1", agents={"worker": worker}
    )
    monkeypatch.setattr("hive.cli.Team", SimpleNamespace(load=lambda name, **kw: team))
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda _p, k: "channel" if k == "hive-remote" else None,
    )
    channel = hive_home / "channel"
    channel.mkdir(parents=True, exist_ok=True)
    stale = channel / "hive-client-1.sock"
    stale.touch()
    fresh = channel / "hive-client-2.sock"
    fresh.touch()
    (channel / "hive-client-2.ready").write_text("2")
    link = claude_channel.channel_socket_path("%50")
    os.symlink(stale, link)

    from hive.cli import _claim_context_session, _existing_ccd_duo

    res = _existing_ccd_duo(str(fresh))

    assert res is not None
    ctx = hive_context.load_current_context()
    assert ctx.get("session", "") == ""
    assert ctx.get("team") == "hive-ccd-w1" and ctx.get("agent") == "worker"
    # The new host session can now claim the inbox.
    assert _claim_context_session("11150968-live-live-live-000000000000") is True


def test_team_status_marks_the_anchored_member(configure_hive_home, monkeypatch):
    """`cliAlive: false` on an anchor pane is by design, so the payload says
    so — a reader must not have to guess whether the member is dead."""
    configure_hive_home()
    from hive.agent import Agent
    from hive.team import Team

    monkeypatch.setattr(
        "hive.team.tmux.get_pane_option",
        lambda pane, key: "channel" if (pane == "%50" and key == "hive-remote") else None,
    )
    team = Team(name="hive-ccd-w1", tmux_session="hive-ccd", tmux_window="hive-ccd:1")
    team.agents["worker"] = Agent(name="worker", team_name=team.name, pane_id="%50", cli="claude")
    team.agents["validator"] = Agent(name="validator", team_name=team.name, pane_id="%51", cli="codex")

    members = {m["name"]: m for m in team.status()["members"]}

    assert members["worker"]["remote"] == "channel"
    assert "remote" not in members["validator"]
