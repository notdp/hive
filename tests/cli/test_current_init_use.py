import json
import os
from types import SimpleNamespace

from hive import bus
from hive.cli import cli


def test_current_reads_persisted_context(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-d", "--workspace", str(workspace)]).exit_code == 0
    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["name"] == "team-d"
    assert payload["runtimeWorkspace"] == str(workspace)
    assert payload["cwd"] == os.getcwd()
    assert payload["self"] == "orch"


def test_current_discovers_tmux_when_no_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "main")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "main:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%0")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%0", "[orch]", command="claude"),
            PaneInfo("%12", "[claude]", command="claude"),
        ],
    )

    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] is None
    assert payload["tmux"]["session"] == "main"
    assert payload["tmux"]["paneCount"] == 2
    assert payload["tmux"]["panes"][0]["id"] == "%0"
    assert payload["tmux"]["panes"][0]["role"] == "agent"
    assert payload["tmux"]["panes"][1]["role"] == "agent"
    # no-team hint points straight at orch init
    assert "hive init" in payload["hint"]


def test_current_ignores_persisted_context_inside_tmux_when_window_is_unbound(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    ctx_dir = tmp_path / ".hive" / "contexts"
    ctx_dir.mkdir(parents=True, exist_ok=True)
    (ctx_dir / "pane-0.json").write_text(json.dumps({"team": "stale-team", "workspace": "/tmp/ws", "agent": "claude"}))

    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] is None
    assert payload["hint"].startswith("No team bound")


def test_current_ignores_window_only_team_binding_without_pane_registration(runner, configure_hive_home, tmp_path):
    configure_hive_home(current_pane="%9", session_name="dev")

    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")

    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] is None
    assert payload["hint"].startswith("No team bound")


def test_team_no_tmux_no_team(runner, configure_hive_home, monkeypatch):
    """Outside tmux with nothing in scope, team asks for -t instead of a tmux."""
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["team"])

    assert result.exit_code != 0
    assert "-t <team>" in result.output


def test_team_explicit_t_works_outside_tmux(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    from hive import registry

    assert registry.record_team(
        team="honey", workspace="/tmp/ws-h", created_at="1.0",
        members=[{"name": "rex", "cli": "grok", "sessionId": "sid-g", "cwd": "/repo"}],
    ) == "written"
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["team", "-t", "honey"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["name"] == "honey"
    rex = next(m for m in payload["members"] if m["name"] == "rex")
    assert rex["pane"] == ""


def test_current_discovers_registered_agent_from_tmux_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%9", session_name="dev")

    # Set up tmux state directly (no more config.json)
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "agent", "orch", "dev")
    tmux.tag_pane("%9", "agent", "alpha", "dev")

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["name"] == "dev"
    assert payload["runtimeWorkspace"] == str(tmp_path / "ws")
    assert payload["self"] == "alpha"
    alpha = next(m for m in payload["members"] if m["name"] == "alpha")
    assert alpha["pane"] == "%9"
    assert payload["tmuxSession"] == "dev"
    assert payload["tmuxWindow"] == "dev:0"
    assert payload["cwd"] == os.getcwd()


def test_current_shows_tagged_role_for_lead_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%0", session_name="dev")

    # Set up tmux state
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "agent", "orch", "dev")

    # Even when the pane command is a shell, self discovery still works off the tmux tag.
    monkeypatch.setattr("hive.cli.tmux.get_pane_current_command", lambda _pane: "python3.12")

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["self"] == "orch"
    orch = next(m for m in payload["members"] if m["name"] == "orch")
    assert orch["pane"] == "%0"


def test_current_returns_tagged_role_regardless_of_tty(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%0", session_name="dev")

    # Set up tmux state
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "agent", "orch", "dev")

    # These overrides don't break self discovery (self is taken from the pane tag).
    monkeypatch.setattr("hive.cli.tmux.get_pane_current_command", lambda _pane: "2.1.88")
    monkeypatch.setattr("hive.cli.tmux.get_pane_title", lambda _pane: "✳ Claude Code")
    monkeypatch.setattr("hive.cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.cli.tmux.list_tty_commands", lambda _tty: ["-zsh", "claude"])

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["self"] == "orch"
    orch = next(m for m in payload["members"] if m["name"] == "orch")
    assert orch["pane"] == "%0"


def test_init_returns_existing_team_for_registered_member(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%9", session_name="dev")

    # Set up tmux state directly (no more config.json)
    from hive import tmux
    tmux.set_window_option("dev:0", "@hive-team", "dev")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    tmux.tag_pane("%0", "agent", "orch", "dev")
    tmux.tag_pane("%9", "agent", "alpha", "dev")

    result = runner.invoke(cli, ["init"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "team": "dev",
        "workspace": str(tmp_path / "ws"),
        "agent": "alpha",
        "role": "agent",
        "pane": "%9",
        "tmuxSession": "dev",
        "tmuxWindow": "dev:0",
    }


def test_init_stops_existing_sidecar_before_auto_workspace_reset(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%5", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_id", lambda: "@2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _pane_id: SimpleNamespace(name="claude"),
    )
    monkeypatch.setattr("hive.cli._ensure_team_sidecar", lambda *_args, **_kwargs: None)
    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:2")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%5", "", command="claude")],
    )

    calls: list[tuple[str, str]] = []

    def _fake_stop(workspace: str) -> None:
        calls.append(("stop", workspace))

    def _fake_reset(workspace):
        calls.append(("reset", str(workspace)))
        return bus.init_workspace(workspace)

    monkeypatch.setattr("hive.sidecar.stop_sidecar", _fake_stop)
    monkeypatch.setattr("hive.cli.bus.reset_workspace", _fake_reset)

    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0
    assert calls[:2] == [
        ("stop", "/tmp/hive-dev-w2"),
        ("reset", "/tmp/hive-dev-w2"),
    ]


def test_init_self_register_never_injects_hive_slash_into_own_input(
    runner, configure_hive_home, monkeypatch, mock_tmux_send, tmp_path,
):
    """Regression: when an agent (e.g. Claude) runs `hive init` in its own
    pane and the window is already bound to a team, `hive init` used to call
    `member.load_skill("hive")` + `member.send(join_message)` on the self
    pane. Those `tmux send-keys` calls landed in the pane's own input queue,
    causing the agent to see a phantom second `/hive` trigger plus a stray
    "You are '<name>'..." prompt once the current turn finished.

    Any bytes sent to the self pane during init is a bug. Check the raw
    `send` stream directly so future refactors of the rebind path cannot
    re-introduce the regression without tripping this test.
    """
    configure_hive_home(current_pane="%42", session_name="main")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "main")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "3")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "main:3")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%42")
    monkeypatch.setattr("hive.cli.secrets.choice", lambda names: "pipi")

    from hive import tmux
    from hive.tmux import PaneInfo

    tmux.set_window_option("main:3", "@hive-team", "main-3")
    tmux.set_window_option("main:3", "@hive-workspace", str(tmp_path / "ws"))
    tmux.set_window_option("main:3", "@hive-created", "0")

    def fake_get_pane_option(pane_id: str, key: str):
        if pane_id == "%42" and key == "hive-team":
            return "main-3"
        return None

    monkeypatch.setattr("hive.cli.tmux.get_pane_option", fake_get_pane_option)
    monkeypatch.setattr("hive.tmux.get_pane_option", fake_get_pane_option)
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%40", "orch", command="claude", role="agent", agent="orch", team="main-3"),
            PaneInfo("%42", "Claude", command="claude", team="main-3"),
        ],
    )
    monkeypatch.setattr(
        "hive.team.tmux.list_panes_full",
        lambda _target: [
            PaneInfo("%40", "orch", command="claude", role="agent", agent="orch", team="main-3"),
            PaneInfo("%42", "Claude", command="claude", team="main-3"),
        ],
    )
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda pane_id: type("P", (), {"name": "claude"})() if pane_id == "%42" else None,
    )

    result = runner.invoke(cli, ["init"])
    assert result.exit_code == 0

    self_sends = [text for pane, text in mock_tmux_send if pane == "%42"]
    assert self_sends == [], (
        f"hive init must not send anything to the pane that launched it, "
        f"but %42 received: {self_sends!r}"
    )


def test_init_replaces_window_only_team_binding_without_members(runner, configure_hive_home, monkeypatch, tmp_path):
    """A window carrying only a stale `@hive-team` tag (no registered self pane)
    is not treated as a binding: init clears the stale tag and binds a fresh
    orch team named after the current window."""
    configure_hive_home(current_pane="%9", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "0")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%9")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))

    from hive import tmux
    from hive.tmux import PaneInfo

    tmux.set_window_option("dev:0", "@hive-team", "ghost")
    tmux.set_window_option("dev:0", "@hive-workspace", str(tmp_path / "ghost-ws"))
    tmux.set_window_option("dev:0", "@hive-created", "0")
    monkeypatch.setattr("hive.cli.tmux.list_panes_full", lambda _target: [PaneInfo("%9", "", command="claude")])
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda *_a, **_k: tmp_path / "ws")

    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    # Stale "ghost" tag cleared; a fresh orch team is bound to this window.
    assert payload["team"] == "honey"
    assert payload["orch"]["name"] == "orch"
    assert payload["protocol"] == "/hive:orch"
    assert tmux.get_window_option("dev:0", "hive-team") == "honey"


def test_init_creates_team_and_binds_orch(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%5", "[orch]", command="claude")],
    )
    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:2")

    workspace = tmp_path / "ws"
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda *_a, **_k: workspace)
    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    # init binds the current pane as orch and spawns nobody.
    assert payload["team"] == "honey"
    assert payload["orch"]["name"] == "orch"
    assert payload["orch"]["pane"] == "%5"
    assert payload["protocol"] == "/hive:orch"

    # The team is created and the current pane is remembered as the orch.
    from hive.team import Team
    assert Team.load("honey").workspace == str(workspace)
    current = json.loads((tmp_path / ".hive" / "contexts" / "pane-5.json").read_text())
    assert current["team"] == "honey"
    assert current["agent"] == "orch"


def test_init_accepts_preopened_codex_orch_pane(
    runner, configure_hive_home, monkeypatch, tmp_path,
):
    """A pre-opened codex CLI in the current pane is a valid orch: init
    detects the codex profile and binds the team."""
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "5")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:5")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%10")
    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:5")

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%10", "Codex", command="zsh")],
    )
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda pane_id: SimpleNamespace(name="codex") if pane_id == "%10" else None,
    )
    # A pre-opened codex must be hive-managed for the init gate to pass: a
    # recorded thread on a live shared daemon short-circuits
    # _require_daemon_backed.
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.thread_id_for_pane", lambda _p: "tid-10"
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.daemon_alive", lambda: True)

    workspace = tmp_path / "ws"
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda *_a, **_k: workspace)
    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "honey"
    assert payload["orch"]["name"] == "orch"
    assert payload["orch"]["cli"] == "codex"


def test_init_removed_options_are_rejected(runner, configure_hive_home, monkeypatch):
    """Removed init options are rejected at the Click parser layer and the
    orch bring-up is never entered."""
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)

    calls: list[object] = []
    monkeypatch.setattr("hive.cli._create_orch_team", lambda **kw: calls.append(kw) or {})

    for argv in (
        ["init", "--name", "my-team"],
        ["init", "-n", "my-team"],
        ["init", "--workspace", "/tmp/x"],
        ["init", "-w", "/tmp/x"],
        ["init", "--notify"],
        ["init", "--no-notify"],
        ["init", "--validator-cli", "codex"],
    ):
        result = runner.invoke(cli, argv)
        assert result.exit_code == 2, (argv, result.output)
        assert "No such option" in result.output, (argv, result.output)
    assert calls == []


def test_init_starts_sidecar_for_new_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_id", lambda: "@2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%5", "", command="claude")],
    )

    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:2")

    calls: list[tuple[str, str, str, str]] = []

    def _fake_ensure_sidecar(workspace_arg: str, team: str, tmux_window: str, tmux_window_id: str):
        calls.append((workspace_arg, team, tmux_window, tmux_window_id))
        return 4321

    monkeypatch.setattr("hive.sidecar.ensure_sidecar", _fake_ensure_sidecar)

    workspace = tmp_path / "ws"
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda *_a, **_k: workspace)
    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0
    assert calls == [(str(workspace), "honey", "dev:2", "@2")]


def test_init_resets_existing_auto_workspace_by_default(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    auto_workspace = tmp_path / "auto-ws"
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda _session, _window, _fallback="0": auto_workspace)

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%5", "", command="claude")],
    )

    bus.init_workspace(auto_workspace)
    (auto_workspace / "artifacts").mkdir(parents=True, exist_ok=True)
    bus.write_event(
        auto_workspace,
        from_agent="orch",
        to_agent="gpt",
        intent="send",
        message_id="old1",
        body="stale",
    )
    (auto_workspace / "artifacts" / "stale.txt").write_text("stale")

    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0
    # Auto workspace is reset before the new team is created: stale events and
    # artifacts from the previous team are gone.
    assert bus.count_events(auto_workspace) == 0
    assert len(list((auto_workspace / "artifacts").iterdir())) == 0


def test_team_gc_removes_leftover_team_dir_for_dead_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home(current_pane="%8", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%8")

    from hive.tmux import PaneInfo

    monkeypatch.setattr("hive.cli.tmux.list_panes_full", lambda _target: [PaneInfo("%8", "", command="claude")])

    # Leftover team dir from a dead team (no corresponding tmux window)
    team_dir = tmp_path / ".hive" / "teams" / "dev-0"
    team_dir.mkdir(parents=True)

    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    # GC removes leftover team dirs not backed by live tmux windows
    assert not team_dir.exists()


def test_init_never_picks_a_registry_claimed_pool_name(
    runner, configure_hive_home, monkeypatch, tmp_path,
):
    # The registry is the name authority: a headless/detached team owns its
    # name until `hive delete` — init's pool pick must skip it, and the
    # existing entry (engines may still run behind it) stays untouched.
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "2")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:2")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))

    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full",
        lambda _target: [PaneInfo("%5", "[orch]", command="claude")],
    )
    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:2")
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda *_a, **_k: tmp_path / "ws")

    from hive import registry
    from hive.cli import TEAM_NAME_POOL

    first_pool_name = TEAM_NAME_POOL[0]
    assert registry.record_team(
        team=first_pool_name, workspace="/tmp/hive-old", created_at="100.0",
        members=[{"name": "worker", "cli": "grok", "sessionId": "LIVE-SID", "cwd": "/repo"}],
    ) == "written"
    before = registry.load(first_pool_name)

    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0, result.output
    picked = json.loads(result.output)["team"]
    assert picked != first_pool_name
    assert registry.load(first_pool_name) == before  # untouched

def test_init_skips_pool_names_claimed_by_live_teams_and_squads(
    runner, configure_hive_home, monkeypatch, tmp_path,
):
    configure_hive_home(current_pane="%8", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_index", lambda: "1")
    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%8")

    from hive.tmux import PaneInfo

    # a live team elsewhere claims the first pool name, a squad namespace
    # prefix claims the second — the new team takes the third
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_all",
        lambda: [
            PaneInfo("%2", "", role="agent", agent="worker", team="honey"),
            PaneInfo("%3", "", role="agent", agent="comb.orch", group="comb"),
        ],
    )
    monkeypatch.setattr("hive.cli.tmux.list_panes_full", lambda _target: [PaneInfo("%8", "", command="claude")])
    monkeypatch.setattr("hive.cli._default_auto_workspace_path", lambda *_a, **_k: tmp_path / "ws-1")

    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["team"] == "wasp"


def test_init_breakout_names_team_from_final_window(runner, configure_hive_home, monkeypatch, tmp_path):
    """Bug A: when the worker breaks out of a crowded window, the team name,
    workspace, and sidecar binding all follow the FINAL window's stable id, not
    the origin window or its (mutable) index."""
    configure_hive_home(current_pane="%5", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    # Origin window dev:0 is crowded (3 panes) → the worker breaks out to dev:3.
    monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: "dev:0")
    from hive.tmux import PaneInfo as _PI
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_full_or_none",
        lambda _t: [_PI("%5", ""), _PI("%6", ""), _PI("%7", "")],
    )
    monkeypatch.setattr("hive.cli.tmux.break_pane", lambda p, **k: ("dev:3", "%200"))
    # The final window's id slug (@42) differs from origin (@0) and index (3),
    # so a passing assertion proves the name is final-window-id-derived.
    monkeypatch.setattr("hive.cli.tmux.get_window_id", lambda target: "@42" if target == "dev:3" else "@0")

    from hive.tmux import PaneInfo

    monkeypatch.setattr("hive.cli.tmux.list_panes_full", lambda _target: [PaneInfo("%5", "", command="claude")])

    sidecar_calls: list[tuple[str, str, str, str]] = []
    monkeypatch.setattr(
        "hive.sidecar.ensure_sidecar",
        lambda ws, team, win, wid: sidecar_calls.append((ws, team, win, wid)) or 1,
    )

    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["team"] == "honey"
    assert payload["window"] == "dev:3"
    # the workspace + sidecar binding carry the final-window probe (@42),
    # not origin (@0) or index (3) — the team name is a pool pick
    assert sidecar_calls == [("/tmp/hive-dev-w42", "honey", "dev:3", "@42")]


def test_init_idempotent_rerun_from_bound_worker_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    """Re-running init from a pane already bound as a worker echoes the existing
    binding without breaking out, clearing window tags, or resetting state."""
    configure_hive_home(current_pane="%5", session_name="dev")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))

    bound = {"team": "dev-w2", "agent": "worker", "role": "agent", "workspace": "/tmp/hive-dev-w2"}
    monkeypatch.setattr("hive.cli._discover_tmux_binding", lambda: bound)

    cleared: list[tuple[str, str]] = []
    resets: list[str] = []
    breaks: list[str] = []
    monkeypatch.setattr("hive.cli.tmux.clear_window_option", lambda wt, key: cleared.append((wt, key)))
    monkeypatch.setattr("hive.cli.bus.reset_workspace", lambda ws: resets.append(str(ws)))
    monkeypatch.setattr("hive.cli.tmux.break_pane", lambda p, **k: breaks.append(p) or ("dev:9", "%900"))

    result = runner.invoke(cli, ["init"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["team"] == "dev-w2"   # echoes the existing binding untouched
    assert cleared == []                  # no window-option clearing
    assert resets == []                   # no workspace reset
    assert breaks == []                   # no break-out


def test_init_fails_outside_tmux(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["init"])
    assert result.exit_code != 0
    assert "tmux" in result.output.lower()


def test_legacy_commands_removed(runner):
    for command in ("comment", "wait", "read", "inbox"):
        result = runner.invoke(cli, [command, "--help"])
        assert result.exit_code != 0
        assert f"No such command '{command}'" in result.output


def test_root_help_groups_commands_by_area(runner):
    result = runner.invoke(cli, ["--help"])

    assert result.exit_code == 0
    output = result.output
    assert "Hive - tmux-first multi-agent collaboration runtime." in output
    for section in (
        "Daily:",
        "Panes:",
        "Workflow:",
        "Team:",
        "Debug:",
        "Human Helpers:",
        "Extensions:",
        "Launchers:",
        "Examples:",
    ):
        assert section in output

    for short_help in (
        "Show team overview.",
        "Show a reply thread rooted at a msgId.",
        "Manage first-party Hive plugins.",
    ):
        assert short_help in output

    # init binds the orch.
    assert "Make the current pane the orch of a fresh team." in output
    # pr / worktree live under Workflow; register / layout under Team.
    for command in ("pr", "worktree", "register", "layout"):
        assert f"  {command} " in output
    # topology commands are gone for good.
    for removed in ("duo", "squad"):
        assert f"  {removed} " not in output
    # terminal / exec are gone for good.
    for removed in ("terminal", "exec"):
        assert f"  {removed} " not in output

    assert "Debug: inject raw input into an agent pane." in output

    for hidden in ("inbox", "status-show", "statuses", "who"):
        assert f"  {hidden} " not in output
    assert "status  Show projected collaboration statuses." not in output
    assert "  type " not in output


def test_layout_applies_preset(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    assert runner.invoke(cli, ["create", "team-lay", "--workspace", str(tmp_path / "ws")]).exit_code == 0

    layouts_applied: list[tuple[str, str]] = []

    def fake_select_layout(target, layout="tiled"):
        layouts_applied.append((target, layout))

    monkeypatch.setattr("hive.cli.tmux.select_layout", fake_select_layout)
    monkeypatch.setattr("hive.cli.tmux.set_window_option", lambda *a, **kw: None)

    result = runner.invoke(cli, ["layout", "tiled"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["layout"] == "tiled"
    assert any(l == "tiled" for _, l in layouts_applied)


def test_layout_rejects_unknown_preset(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    assert runner.invoke(cli, ["create", "team-lay2", "--workspace", str(tmp_path / "ws")]).exit_code == 0

    result = runner.invoke(cli, ["layout", "bogus"])
    assert result.exit_code != 0


# --- transactional registration (VAL fail-r1 finding 3) ---


def _register_probe_team(tmp_path):
    from types import SimpleNamespace

    return SimpleNamespace(
        name="team-x",
        workspace=str(tmp_path / "ws"),
        agents={},
    )


def test_register_rolls_back_everything_when_native_join_refused(
    configure_hive_home, monkeypatch, tmp_path
):
    """A pane whose native transport refuses the join message must not linger
    half-registered: no member entry, no tmux tags, no saved pane context."""
    import pytest

    import hive.cli as cli_mod
    from hive.agent import DeliveryError
    from hive import context as hive_context

    configure_hive_home()
    t = _register_probe_team(tmp_path)
    tags: list = []
    cleared: list = []
    monkeypatch.setattr(cli_mod.tmux, "tag_pane", lambda *a, **k: tags.append(a))
    monkeypatch.setattr(cli_mod.tmux, "clear_pane_tags", lambda pane: cleared.append(pane))

    def _refuse(self, text):
        raise DeliveryError("no transport")

    monkeypatch.setattr("hive.agent.Agent.send", _refuse)

    with pytest.raises(SystemExit):
        cli_mod._register_agent_member(
            t,
            pane_id="%42",
            team_name="team-x",
            agent_name="new",
            pane_cli="codex",
            cwd="/tmp",
            notify=True,
        )

    assert t.agents == {}                      # member entry rolled back
    assert cleared == ["%42"]                  # tmux tags rolled back
    ctx = hive_context.CONTEXT_DIR / "pane-42.json"
    assert not ctx.exists()                    # saved context rolled back

    # a later retry starts clean and succeeds
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: "udsWriteAccepted")
    agent = cli_mod._register_agent_member(
        t,
        pane_id="%42",
        team_name="team-x",
        agent_name="new",
        pane_cli="codex",
        cwd="/tmp",
        notify=True,
    )
    assert t.agents["new"] is agent


def test_register_no_notify_registers_without_reachability_proof(
    configure_hive_home, monkeypatch, tmp_path
):
    """--no-notify is the deliberate escape hatch: it registers a pane without
    proving the native transport deliverable (documented in the option help)."""
    import hive.cli as cli_mod

    configure_hive_home()
    t = _register_probe_team(tmp_path)
    monkeypatch.setattr(cli_mod.tmux, "tag_pane", lambda *a, **k: None)

    def _boom(self, text):
        raise AssertionError("no-notify must not touch the transport")

    monkeypatch.setattr("hive.agent.Agent.send", _boom)
    agent = cli_mod._register_agent_member(
        t,
        pane_id="%43",
        team_name="team-x",
        agent_name="loner",
        pane_cli="codex",
        cwd="/tmp",
        notify=False,
    )
    assert t.agents["loner"] is agent


def test_create_outside_tmux_registers_headless_team(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    ws = tmp_path / "ws-headless"

    result = runner.invoke(cli, ["create", "honey", "--workspace", str(ws)])

    assert result.exit_code == 0, result.output
    assert "headless" in result.output
    from hive import registry

    entry = registry.load("honey")
    assert entry is not None
    assert entry["workspace"] == str(ws)
    assert entry["members"] == []
    assert (ws / "state").is_dir()  # workspace initialized

    # a second create of the same name refuses instead of clobbering
    again = runner.invoke(cli, ["create", "honey"])
    assert again.exit_code != 0
    assert "already exists" in again.output


def test_create_outside_tmux_rejects_reserved_names(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    assert runner.invoke(cli, ["create", "ccd"]).exit_code != 0
    assert runner.invoke(cli, ["create", "a.b"]).exit_code != 0
