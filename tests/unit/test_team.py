from hive.agent import Agent
from hive.team import Team, Terminal


def test_terminal_to_dict_uses_liveness(monkeypatch):
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda pane_id: pane_id == "%42")
    terminal = Terminal(name="shell", pane_id="%42")

    assert terminal.to_dict() == {"name": "shell", "tmuxPaneId": "%42", "isActive": True}


def test_team_create_inside_tmux_tags_lead_and_detects_session(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%7")
    tagged = []
    monkeypatch.setattr("hive.team.detect_current_session_id", lambda _cwd: "sess-123")
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *args: tagged.append(args))

    team = Team.create("team-a", description="demo", workspace="/tmp/ws")

    assert team.lead_pane_id == "%7"
    assert team.lead_session_id == "sess-123"
    assert tagged == [("%7", "lead", "orchestrator", "team-a")]


def test_team_create_outside_tmux_creates_session(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    calls = []
    monkeypatch.setattr("hive.team.tmux.has_session", lambda _name: False)
    monkeypatch.setattr("hive.team.tmux.new_session", lambda name: calls.append(name) or "%0")

    team = Team.create("team-a")

    assert team.lead_pane_id == ""
    assert calls == ["team-a"]


def test_team_create_rejects_duplicate_outside_tmux(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr("hive.team.tmux.has_session", lambda _name: True)

    try:
        Team.create("team-a")
    except ValueError as exc:
        assert "already exists" in str(exc)
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
    )
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1", model="m1", color="cyan", cwd="/tmp")
    team.terminals["shell"] = Terminal(name="shell", pane_id="%2")

    team.save()
    loaded = Team.load("team-a")

    assert loaded.name == "team-a"
    assert loaded.description == "demo"
    assert loaded.lead_session_id == "sess-1"
    assert loaded.agents["claude"].pane_id == "%1"
    assert loaded.terminals["shell"].pane_id == "%2"


def test_team_lead_agent_uses_persisted_session_id(configure_hive_home):
    configure_hive_home()
    team = Team(name="team-a", lead_pane_id="%0", lead_session_id="sess-1")

    lead = team.lead_agent()

    assert lead is not None
    assert lead.name == "orchestrator"
    assert lead.session_id == "sess-1"


def test_team_spawn_tags_agent_and_loads_workflow(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%0")
    spawned = []
    tagged = []
    layouts = []

    agent = Agent(name="claude", team_name="team-a", pane_id="%9", color="green")
    monkeypatch.setattr(
        "hive.team.Agent.spawn",
        lambda **kwargs: spawned.append(kwargs) or agent,
    )
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *args: tagged.append(args))
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.team.tmux.enable_pane_border_status", lambda target: layouts.append(("border", target)))
    monkeypatch.setattr("hive.team.tmux.set_window_option", lambda target, option, value: layouts.append((target, option, value)))
    monkeypatch.setattr("hive.team.tmux.select_layout", lambda target, layout: layouts.append(("layout", target, layout)))
    monkeypatch.setattr("hive.agent.Agent.load_skill", lambda self, workflow: setattr(self, "loaded_workflow", workflow))

    team = Team(name="team-a", lead_pane_id="%0")
    result = team.spawn("claude", workflow="cross-review")

    assert result is agent
    assert spawned[0]["target_pane"] == "%0"
    assert spawned[0]["color"] == "green"
    assert tagged == [("%9", "agent", "claude", "team-a")]
    assert getattr(agent, "loaded_workflow") == "cross-review"
    assert ("border", "dev:1") in layouts


def test_team_spawn_second_agent_splits_from_last_agent(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%0")
    calls = []
    monkeypatch.setattr(
        "hive.team.Agent.spawn",
        lambda **kwargs: calls.append(kwargs) or Agent(name=kwargs["name"], team_name="team-a", pane_id=f"%{len(calls)+8}", color=kwargs["color"]),
    )
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *_args: None)
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: None)

    team = Team(name="team-a", lead_pane_id="%0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%9", color="green")
    team.spawn("gpt")

    assert calls[0]["target_pane"] == "%9"
    assert calls[0]["split_horizontal"] is False
    assert calls[0]["color"] == "blue"


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

    assert team.get("orchestrator").pane_id == "%0"
    assert team.get("claude") is alive
    team.broadcast("hello", exclude="gpt")
    assert sent == [("claude", "hello")]


def test_team_status_and_is_tmux_alive(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.has_session", lambda name: name == "dev")
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda pane: pane != "%dead")
    team = Team(name="team-a", workspace="/tmp/ws", lead_pane_id="%0", lead_session_id="sess-1", tmux_session="dev")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1", model="m1", color="cyan")
    team.terminals["shell"] = Terminal(name="shell", pane_id="%2")

    payload = team.status()

    assert payload["agents"]["orchestrator"]["sessionId"] == "sess-1"
    assert payload["agents"]["claude"]["model"] == "m1"
    assert payload["terminals"]["shell"]["pane"] == "%2"
    assert team.is_tmux_alive() is True
    team.lead_pane_id = "%dead"
    assert team.is_tmux_alive() is False


def test_team_shutdown_and_cleanup(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    calls = []
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
    monkeypatch.setattr("hive.team.tmux.clear_pane_tags", lambda pane: calls.append(("clear", pane)))
    monkeypatch.setattr("hive.team.tmux.kill_session", lambda name: calls.append(("kill-session", name)))
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
    assert ("kill-session", "team-a") in calls
