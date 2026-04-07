from hive import tmux as _tmux
from hive.agent import Agent
from hive.team import Team, Terminal, _find_team_window, _gc_stale_team_windows


def test_terminal_to_dict_uses_liveness(monkeypatch):
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda pane_id: pane_id == "%42")
    terminal = Terminal(name="shell", pane_id="%42")

    assert terminal.to_dict() == {"name": "shell", "tmuxPaneId": "%42", "isActive": True}


def test_team_create_inside_tmux_tags_lead_and_detects_session(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%7")
    tagged = []
    monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": "sess-123")
    monkeypatch.setattr("hive.team.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *args: tagged.append(args))

    team = Team.create("team-a", description="demo", workspace="/tmp/ws")

    assert team.lead_pane_id == "%7"
    assert team.lead_session_id == "sess-123"
    assert team.tmux_session == "dev"
    assert team.tmux_window == "dev:0"
    assert tagged == [("%7", "agent", "orch", "team-a")]


def test_team_create_rejects_outside_tmux(configure_hive_home):
    configure_hive_home(tmux_inside=False)

    try:
        Team.create("team-a")
    except ValueError as exc:
        assert "requires tmux" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_team_save_and_load_round_trip(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
    team = Team(
        name="team-a",
        description="demo",
        workspace="/tmp/ws",
        lead_pane_id="%0",
        lead_session_id="sess-1",
        tmux_session="dev",
        tmux_window="dev:0",
    )
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1", model="m1", color="cyan", cwd="/tmp")
    team.terminals["shell"] = Terminal(name="shell", pane_id="%2")

    team.save()

    # Set up pane tags for load to find (in real usage, set during create/spawn)
    _tmux.tag_pane("%0", "lead", "orch", "team-a")
    _tmux.tag_pane("%1", "agent", "claude", "team-a", model="m1", color="cyan")
    _tmux.tag_pane("%2", "terminal", "shell", "team-a")

    loaded = Team.load("team-a")

    assert loaded.name == "team-a"
    assert loaded.description == "demo"
    assert loaded.tmux_window == "dev:0"
    assert loaded.lead_pane_id == "%0"
    assert loaded.agents["claude"].pane_id == "%1"
    assert loaded.terminals["shell"].pane_id == "%2"


def test_team_lead_agent_uses_persisted_session_id(configure_hive_home):
    configure_hive_home()
    team = Team(name="team-a", lead_pane_id="%0", lead_session_id="sess-1")

    lead = team.lead_agent()

    assert lead is not None
    assert lead.name == "orch"
    assert lead.session_id == "sess-1"


def test_team_spawn_tags_agent_and_passes_workflow_as_initial_skill(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%0")
    spawned = []
    tagged = []
    layouts = []
    sent = []

    agent = Agent(name="claude", team_name="team-a", pane_id="%9", color="green")
    monkeypatch.setattr(
        "hive.team.Agent.spawn",
        lambda **kwargs: spawned.append(kwargs) or agent,
    )
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *args, **kwargs: tagged.append(args))
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.team.tmux.enable_pane_border_status", lambda target: layouts.append(("border", target)))
    monkeypatch.setattr("hive.team.tmux.set_window_option", lambda target, option, value: layouts.append((target, option, value)))
    monkeypatch.setattr("hive.team.tmux.select_layout", lambda target, layout: layouts.append(("layout", target, layout)))
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: sent.append(text))

    team = Team(name="team-a", lead_pane_id="%0")
    result = team.spawn("claude", workflow="code-review", prompt="start now")

    assert result is agent
    assert spawned[0]["target_pane"] == "%0"
    assert spawned[0]["color"] == "green"
    assert spawned[0]["skill"] == "code-review"
    assert spawned[0]["prompt"] == "start now"
    assert spawned[0]["send_bootstrap_prompt"] is False
    assert tagged == [("%9", "agent", "claude", "team-a")]
    assert sent == []
    assert ("border", "dev:1") in layouts


def test_team_spawn_second_agent_splits_from_last_agent(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%0")
    calls = []
    monkeypatch.setattr(
        "hive.team.Agent.spawn",
        lambda **kwargs: calls.append(kwargs) or Agent(name=kwargs["name"], team_name="team-a", pane_id=f"%{len(calls)+8}", color=kwargs["color"]),
    )
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *_args, **_kwargs: None)
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: None)

    team = Team(name="team-a", lead_pane_id="%0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%9", color="green")
    team.spawn("gpt")

    assert calls[0]["target_pane"] == "%9"
    assert calls[0]["split_horizontal"] is False
    assert calls[0]["color"] == "blue"
    assert calls[0]["skill"] == "hive"
    assert calls[0]["send_bootstrap_prompt"] is True


def test_team_get_and_broadcast(configure_hive_home, monkeypatch):
    configure_hive_home()
    sent = []
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
    alive = Agent(name="claude", team_name="team-a", pane_id="%1")
    dead = Agent(name="gpt", team_name="team-a", pane_id="%2")
    monkeypatch.setattr(alive, "is_alive", lambda: True)
    monkeypatch.setattr(dead, "is_alive", lambda: False)
    monkeypatch.setattr(alive, "send", lambda text: sent.append(("claude", text)))
    monkeypatch.setattr(dead, "send", lambda text: sent.append(("gpt", text)))

    team = Team(name="team-a", lead_pane_id="%0")
    team.agents = {"claude": alive, "gpt": dead}

    assert team.get("orch").pane_id == "%0"
    assert team.get("claude") is alive
    team.broadcast("hello", exclude="gpt")
    assert sent == [("claude", "hello")]


def test_team_status_and_is_tmux_alive(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.get_pane_tty", lambda _pane: None)
    monkeypatch.setattr("hive.team.resolve_session_id_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.team.tmux.has_session", lambda name: name == "dev")
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda pane: pane != "%dead")
    monkeypatch.setattr(
        "hive.team.tmux.get_pane_current_command",
        lambda pane: {"%0": "python3.12", "%1": "droid", "%2": "zsh"}.get(pane, ""),
    )
    team = Team(name="team-a", workspace="/tmp/ws", lead_pane_id="%0", lead_session_id="sess-1", tmux_session="dev")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1", model="m1", color="cyan")
    team.terminals["shell"] = Terminal(name="shell", pane_id="%2")

    payload = team.status()

    assert payload["tmuxSession"] == "dev"
    assert payload["tmuxWindow"] == ""
    orch = next(member for member in payload["members"] if member["name"] == "orch")
    claude = next(member for member in payload["members"] if member["name"] == "claude")
    shell = next(member for member in payload["members"] if member["name"] == "shell")
    assert orch["sessionId"] == "sess-1"
    assert orch["role"] == "terminal"
    assert claude["model"] == "m1"
    assert claude["role"] == "agent"
    assert shell["pane"] == "%2"
    assert shell["role"] == "terminal"
    assert team.is_tmux_alive() is True
    team.lead_pane_id = "%dead"
    assert team.is_tmux_alive() is False


def test_team_status_backfills_missing_session_ids_from_map(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
    monkeypatch.setattr("hive.team.tmux.get_pane_current_command", lambda pane: "droid" if pane == "%1" else "zsh")
    monkeypatch.setattr(
        "hive.team.resolve_session_id_for_pane",
        lambda pane_id: {"%0": "lead-sess", "%1": "agent-sess"}.get(pane_id),
    )

    team = Team(name="team-a", lead_pane_id="%0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1")

    payload = team.status()

    orch = next(member for member in payload["members"] if member["name"] == "orch")
    claude = next(member for member in payload["members"] if member["name"] == "claude")
    assert orch["sessionId"] == "lead-sess"
    assert claude["sessionId"] == "agent-sess"
    assert team.lead_session_id == "lead-sess"
    assert team.agents["claude"].session_id == "agent-sess"


def test_team_shutdown_and_cleanup(configure_hive_home, monkeypatch):
    configure_hive_home()
    calls = []
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
    monkeypatch.setattr("hive.team.tmux.clear_pane_tags", lambda pane: calls.append(("clear", pane)))
    a1 = Agent(name="claude", team_name="team-a", pane_id="%1")
    a2 = Agent(name="gpt", team_name="team-a", pane_id="%2")
    monkeypatch.setattr(a1, "shutdown", lambda: calls.append(("shutdown", "%1")))
    monkeypatch.setattr(a2, "shutdown", lambda: calls.append(("shutdown", "%2")))
    monkeypatch.setattr(a1, "kill", lambda: calls.append(("kill", "%1")))
    monkeypatch.setattr(a2, "kill", lambda: calls.append(("kill", "%2")))
    team = Team(name="team-a", lead_pane_id="%0")
    team.agents = {"claude": a1, "gpt": a2}
    team.terminals["shell"] = Terminal(name="shell", pane_id="%3")

    team.shutdown("claude")
    team.shutdown()
    team.cleanup()

    assert calls[:3] == [("shutdown", "%1"), ("shutdown", "%1"), ("shutdown", "%2")]
    assert ("kill", "%1") in calls and ("kill", "%2") in calls
    assert ("clear", "%3") in calls and ("clear", "%0") in calls


def test_find_team_window_prefers_pane_window_on_duplicate(configure_hive_home, monkeypatch):
    """When two windows claim the same team, the one containing prefer_pane wins."""
    configure_hive_home()

    list_output = "dev:2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\tmy-team\t/tmp/ws\tdesc\t0\n"
    monkeypatch.setattr(
        "hive.team.tmux._run",
        lambda args, check=True: type("R", (), {"stdout": list_output, "returncode": 0})(),
    )
    monkeypatch.setattr("hive.team.tmux.get_pane_window_target", lambda pane: "dev:3" if pane == "%99" else None)
    cleared: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append((wt, key)))

    wt, data = _find_team_window("my-team", prefer_pane="%99")

    assert wt == "dev:3"
    assert any(wt_c == "dev:2" for wt_c, _ in cleared)


def test_find_team_window_falls_back_to_tagged_panes(configure_hive_home, monkeypatch):
    """Without prefer_pane, pick the window that actually has tagged panes."""
    configure_hive_home()

    list_output = "dev:2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\tmy-team\t/tmp/ws\tdesc\t0\n"
    monkeypatch.setattr(
        "hive.team.tmux._run",
        lambda args, check=True: type("R", (), {"stdout": list_output, "returncode": 0})(),
    )
    monkeypatch.setattr("hive.team.tmux.get_pane_window_target", lambda _pane: None)

    from hive.tmux import PaneInfo
    def fake_list_panes(target):
        if target == "dev:3":
            return [PaneInfo("%50", "", "droid", role="agent", agent="rev-a", team="my-team")]
        return [PaneInfo("%40", "", "droid", role="", agent="", team="")]

    monkeypatch.setattr("hive.team.tmux.list_panes_full", fake_list_panes)
    cleared: list[str] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append(wt))

    wt, _ = _find_team_window("my-team")

    assert wt == "dev:3"
    assert "dev:2" in cleared


def test_gc_stale_team_windows_clears_non_kept(configure_hive_home, monkeypatch):
    configure_hive_home()
    cleared: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append((wt, key)))

    _gc_stale_team_windows("my-team", keep="dev:3", all_windows=["dev:2", "dev:3", "dev:4"])

    stale_windows = {wt for wt, _ in cleared}
    assert stale_windows == {"dev:2", "dev:4"}
    assert ("dev:3", "@hive-team") not in cleared
