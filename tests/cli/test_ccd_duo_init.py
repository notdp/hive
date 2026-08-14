"""Desktop-led duo formation: `hive duo init` outside tmux.

The external Claude session (Claude Code desktop) becomes the worker; its tmux
presence is an anchor pane tagged with the session's own inbox socket, which
the host exports to every child process as ``CLAUDE_CODE_MESSAGING_SOCKET``.
These tests are hermetic: tmux is the conftest fake, the validator spawn is
stubbed, and the host socket is a real listening unix socket (liveness is a
connect, so a plain file would not do).
"""
import json
import socket
import tempfile
from pathlib import Path
from types import SimpleNamespace

import pytest

from hive.cli import cli
from hive import context as hive_context
from hive.adapters import claude_uds


@pytest.fixture
def host_session(monkeypatch):
    """A live inbox socket exported the way a Claude session exports it, plus
    user settings that accept inbound peer messages."""
    made: list = []

    def _configure(*, accept: bool = True, live: bool = True) -> str:
        root = Path(tempfile.mkdtemp(prefix="cc", dir="/tmp"))
        made.append(root)
        settings = root / "settings.json"
        settings.write_text(json.dumps({"crossSessionInbound": "accept"} if accept else {}))
        monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(root))
        sock_path = root / "s.sock"
        if live:
            srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            srv.bind(str(sock_path))
            srv.listen(1)
            made.append(srv)
        monkeypatch.setenv(claude_uds.ENV_SOCKET, str(sock_path))
        return str(sock_path)

    yield _configure
    import shutil

    for item in made:
        if isinstance(item, socket.socket):
            item.close()
        else:
            shutil.rmtree(item, ignore_errors=True)


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
    runner, configure_hive_home, ccd_tmux, host_session
):
    configure_hive_home(tmux_inside=False)
    calls = ccd_tmux()
    sock = host_session()

    result = runner.invoke(cli, ["duo", "init"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["worker"] == {
        "pane": "%50",
        "name": "worker",
        "cli": "claude",
        "remote": "uds",
    }
    assert payload["validator"]["pane"] == "%51"
    assert payload["watch"] == "tmux attach -t hive-ccd"
    assert calls["new_session"] == "hive-ccd"
    assert calls["killed"] == []

    # Anchor pane carries member tags, the remote marker, and the endpoint the
    # host handed us — no discovery formula of ours to go stale.
    opts = {(k, v) for _p, k, v in calls["pane_options"]}
    assert ("hive-agent", "worker") in opts
    assert ("hive-cli", "claude") in opts
    assert ("hive-remote", "uds") in opts
    assert (claude_uds.ENDPOINT_OPTION, sock) in opts

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


def test_duo_init_outside_tmux_refuses_a_dead_host_socket(
    runner, configure_hive_home, ccd_tmux, host_session
):
    """A socket file left behind by a killed session must not form a duo whose
    every send would fail — the check is a connect, not an exists()."""
    configure_hive_home(tmux_inside=False)
    calls = ccd_tmux()
    host_session(live=False)

    result = runner.invoke(cli, ["duo", "init"])

    assert result.exit_code != 0
    assert "nothing is listening" in result.output
    assert calls["killed"] == []  # refused before any window was built
    assert hive_context.load_current_context().get("team", "") == ""


def test_duo_init_outside_tmux_refuses_a_session_that_would_hold_messages(
    runner, configure_hive_home, ccd_tmux, host_session
):
    """Without `crossSessionInbound: accept`, every hive message waits behind a
    human click. Preflight it loudly instead of forming a silently stalled duo."""
    configure_hive_home(tmux_inside=False)
    calls = ccd_tmux()
    host_session(accept=False)

    result = runner.invoke(cli, ["duo", "init"])

    assert result.exit_code != 0
    assert "crossSessionInbound" in result.output
    assert calls["killed"] == []


def _anchor_row(**kw) -> dict[str, str]:
    row = {"pane": "%50", "team": "hive-ccd-w1", "agent": "worker",
           "remote": "uds", "endpoint": "/tmp/me.sock", "cwd": "/tmp/proj"}
    row.update(kw)
    return row


def test_send_outside_tmux_resolves_by_anchor_binding(runner, configure_hive_home, monkeypatch):
    """Outside tmux, identity is the anchor pane recording MY inbox socket —
    so a desktop-led worker can `hive send` without being inside tmux."""
    configure_hive_home(tmux_inside=False)
    monkeypatch.setenv(claude_uds.ENV_SOCKET, "/tmp/me.sock")
    monkeypatch.setattr("hive.tmux.list_remote_members", lambda: [_anchor_row()])
    seen: dict[str, object] = {}

    def _fake_resolve(team, required=True):
        seen["team"] = team
        raise SystemExit(3)  # stop before any tmux resolution

    monkeypatch.setattr("hive.cli._resolve_scoped_team", _fake_resolve)

    result = runner.invoke(cli, ["send", "validator", "hello"])

    # The command got past the root tmux gate and asked for the bound team.
    assert result.exit_code == 3
    assert seen["team"] is None or seen["team"] == "hive-ccd-w1"


def test_send_outside_tmux_with_someone_elses_duo_is_rejected(runner, configure_hive_home, monkeypatch):
    """The live hijack (2026-08-14): a second desktop session in another
    project must NOT inherit the first session's duo. Its socket matches no
    anchor, so the root gate rejects instead of resolving a foreign team."""
    configure_hive_home(tmux_inside=False)
    monkeypatch.setenv(claude_uds.ENV_SOCKET, "/tmp/other-session.sock")
    monkeypatch.setattr("hive.tmux.list_remote_members", lambda: [_anchor_row()])
    # The old resolution path: a stale global context naming the first duo.
    hive_context.save_current_context(team="hive-ccd-w1", workspace="/tmp/ws", agent="worker")

    result = runner.invoke(cli, ["send", "validator", "hello"])

    assert result.exit_code != 0
    assert "not a member" in result.output
    assert "hive duo init" in result.output


def _stub_team(monkeypatch) -> dict[str, object]:
    calls: dict[str, object] = {"zoomed": None}
    worker = SimpleNamespace(pane_id="%50", cli="claude")
    validator = SimpleNamespace(pane_id="%51", cli="codex")
    team = SimpleNamespace(
        name="hive-ccd-w1", tmux_window="hive-ccd:1",
        agents={"worker": worker, "validator": validator},
    )
    monkeypatch.setattr("hive.cli.Team", SimpleNamespace(load=lambda name, **kw: team))
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required=True: "/tmp/ws")
    monkeypatch.setattr("hive.cli.tmux.zoom_pane", lambda p: calls.__setitem__("zoomed", p))
    return calls


def test_existing_ccd_duo_repoints_a_restarted_session(configure_hive_home, monkeypatch):
    """A desktop session restart changes its pid-keyed socket; re-running init
    in the same project repoints that project's anchor and keeps the team."""
    import os

    configure_hive_home(tmux_inside=False)
    calls = _stub_team(monkeypatch)
    monkeypatch.setattr(
        "hive.tmux.list_remote_members",
        lambda: [_anchor_row(endpoint="/tmp/old.sock", cwd=os.getcwd())],
    )
    written: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        "hive.cli.tmux.set_pane_option", lambda p, k, v: written.append((p, k, v))
    )

    from hive.cli import _existing_ccd_duo

    res = _existing_ccd_duo("/tmp/new.sock")

    assert res is not None and res.get("relinked") is True
    assert written == [("%50", claude_uds.ENDPOINT_OPTION, "/tmp/new.sock")]
    assert calls["zoomed"] == "%51"  # anchor stays hidden behind the validator


def test_existing_ccd_duo_adopts_by_endpoint_without_repointing(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _stub_team(monkeypatch)
    monkeypatch.setattr(
        "hive.tmux.list_remote_members",
        lambda: [_anchor_row(endpoint="/tmp/me.sock", cwd="/anywhere")],
    )
    monkeypatch.setattr(
        "hive.cli.tmux.set_pane_option",
        lambda *_a: (_ for _ in ()).throw(AssertionError("no repoint on an endpoint match")),
    )

    from hive.cli import _existing_ccd_duo

    res = _existing_ccd_duo("/tmp/me.sock")

    assert res is not None and "relinked" not in res


def test_existing_ccd_duo_ignores_another_projects_duo(configure_hive_home, monkeypatch):
    """Different socket AND different cwd: that duo belongs to another desktop
    session. Init must fall through to a fresh form, never adopt or repoint."""
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr(
        "hive.tmux.list_remote_members",
        lambda: [_anchor_row(endpoint="/tmp/theirs.sock", cwd="/their/project")],
    )
    monkeypatch.setattr(
        "hive.cli.tmux.set_pane_option",
        lambda *_a: (_ for _ in ()).throw(AssertionError("must not touch a foreign anchor")),
    )

    from hive.cli import _existing_ccd_duo

    assert _existing_ccd_duo("/tmp/mine.sock") is None


def test_team_status_marks_the_anchored_member(configure_hive_home, monkeypatch):
    """`cliAlive: false` on an anchor pane is by design, so the payload says
    so — a reader must not have to guess whether the member is dead."""
    configure_hive_home()
    from hive.agent import Agent
    from hive.team import Team

    monkeypatch.setattr(
        "hive.team.tmux.get_pane_option",
        lambda pane, key: "uds" if (pane == "%50" and key == "hive-remote") else None,
    )
    team = Team(name="hive-ccd-w1", tmux_session="hive-ccd", tmux_window="hive-ccd:1")
    team.agents["worker"] = Agent(name="worker", team_name=team.name, pane_id="%50", cli="claude")
    team.agents["validator"] = Agent(name="validator", team_name=team.name, pane_id="%51", cli="codex")

    members = {m["name"]: m for m in team.status()["members"]}

    assert members["worker"]["remote"] == "uds"
    assert "remote" not in members["validator"]


def test_team_outside_tmux_without_binding_returns_bootstrap_payload(
    runner, configure_hive_home, monkeypatch
):
    """A desktop session sizing up whether to init must be able to look:
    `hive team` passes the root gate unbound and answers with the bootstrap
    payload instead of a tmux lecture (the post-hijack-fix regression)."""
    configure_hive_home(tmux_inside=False)
    monkeypatch.setenv(claude_uds.ENV_SOCKET, "/tmp/nobody.sock")
    monkeypatch.setattr("hive.tmux.list_remote_members", lambda: [_anchor_row()])

    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.output)["team"] is None
