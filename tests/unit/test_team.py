from hive import tmux as _tmux
from hive.agent import Agent
from hive.team import Team, _find_team_window, _gc_stale_team_windows


def test_team_create_inside_tmux_tags_lead_and_detects_session(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%7")
    tagged = []
    borders = []
    monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": "sess-123")
    monkeypatch.setattr("hive.team.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *args: tagged.append(args))
    monkeypatch.setattr("hive.team.tmux.enable_pane_border_status", lambda target: borders.append(target))

    team = Team.create("team-a", description="demo", workspace="/tmp/ws")

    assert team.lead_pane_id == "%7"
    assert team.lead_session_id == "sess-123"
    assert team.tmux_session == "dev"
    assert team.tmux_window == "dev:0"
    assert team.tmux_window_id == "@0"
    assert tagged == [("%7", "agent", "orch", "team-a")]
    assert borders == ["dev:0"]
    assert _tmux.get_window_option("dev:0", "monitor-activity") == "off"
    assert _tmux.get_window_option("dev:0", "monitor-bell") == "off"


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
    borders = []
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
    monkeypatch.setattr("hive.team.tmux.enable_pane_border_status", lambda target: borders.append(target))
    team = Team(
        name="team-a",
        description="demo",
        workspace="/tmp/ws",
        lead_pane_id="%0",
        lead_session_id="sess-1",
        tmux_session="dev",
        tmux_window="dev:0",
        peer_map={"orch": "claude", "claude": "orch"},
    )
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1", model="m1", cwd="/tmp")

    team.save()
    assert borders == ["dev:0"]

    # Set up pane tags for load to find (in real usage, set during create/spawn)
    _tmux.tag_pane("%0", "lead", "orch", "team-a")
    _tmux.tag_pane("%1", "agent", "claude", "team-a", cli="claude")

    loaded = Team.load("team-a")

    assert loaded.name == "team-a"
    assert loaded.description == "demo"
    assert loaded.tmux_window == "dev:0"
    assert loaded.tmux_window_id == "@0"
    assert loaded.lead_pane_id == "%0"
    assert loaded.agents["claude"].pane_id == "%1"
    assert loaded.peer_map == {"orch": "claude", "claude": "orch"}


def test_team_load_restores_agent_cwd_from_pane_current_path(configure_hive_home, monkeypatch):
    configure_hive_home()
    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.team._find_team_window",
        lambda name, prefer_pane="": ("dev:0", {"desc": "", "workspace": "/tmp/ws", "created": "0"}),
    )
    monkeypatch.setattr(
        "hive.team.tmux.list_panes_full",
        lambda _target: [PaneInfo("%1", "", "claude", role="agent", agent="claude", team="team-a", cli="claude")],
    )
    monkeypatch.setattr(
        "hive.team.tmux.display_value",
        lambda pane_id, fmt: "/repo" if pane_id == "%1" and fmt == "#{pane_current_path}" else None,
    )

    loaded = Team.load("team-a")

    assert loaded.agents["claude"].cwd == "/repo"


def test_team_lead_agent_uses_persisted_session_id(configure_hive_home):
    configure_hive_home()
    team = Team(name="team-a", lead_pane_id="%0", lead_session_id="sess-1")

    lead = team.lead_agent()

    assert lead is not None
    assert lead.name == "orch"
    assert lead.session_id == "sess-1"


def test_team_resolve_peer_implicit_for_two_agent_team(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.member_role_for_pane", lambda _pane_id: "agent")
    team = Team(name="team-a", lead_pane_id="%0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1")

    assert team.peer_mode() == "implicit"
    assert team.resolve_peer("orch") == "claude"
    assert team.resolve_peer("claude") == "orch"
    assert team.peer_pairs() == [("claude", "orch")]


def test_team_implicit_pair_returns_two_members_in_implicit_mode(
    configure_hive_home, monkeypatch
):
    """`implicit_pair()` exposes the auto-pair so callers can freeze it into
    explicit before a 3rd agent joins (otherwise peer_mode flips to `none`
    and the displayed relationship vanishes)."""
    configure_hive_home()
    monkeypatch.setattr("hive.team.member_role_for_pane", lambda _pane_id: "agent")
    team = Team(name="team-a", lead_pane_id="%0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1")

    pair = team.implicit_pair()
    assert pair is not None
    assert set(pair) == {"orch", "claude"}


def test_team_implicit_pair_none_when_explicit_or_none_mode(
    configure_hive_home, monkeypatch
):
    configure_hive_home()
    monkeypatch.setattr("hive.team.member_role_for_pane", lambda _pane_id: "agent")

    # Solo team → `none` mode.
    solo = Team(name="solo", lead_pane_id="%0")
    assert solo.peer_mode() == "none"
    assert solo.implicit_pair() is None

    # 3-agent team with no explicit → `none` mode.
    triad = Team(name="triad", lead_pane_id="%0")
    triad.agents["alice"] = Agent(name="alice", team_name="triad", pane_id="%1")
    triad.agents["bob"] = Agent(name="bob", team_name="triad", pane_id="%2")
    assert triad.peer_mode() == "none"
    assert triad.implicit_pair() is None

    # 2-agent team with explicit pair → `explicit` mode.
    explicit = Team(name="explicit", lead_pane_id="%0")
    explicit.agents["claude"] = Agent(name="claude", team_name="explicit", pane_id="%1")
    explicit.set_peer("orch", "claude")
    assert explicit.peer_mode() == "explicit"
    assert explicit.implicit_pair() is None


def test_team_set_peer_is_symmetric_and_clears_previous_mapping(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.member_role_for_pane", lambda _pane_id: "agent")
    team = Team(name="team-a", lead_pane_id="%0", tmux_window="dev:0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1")
    team.agents["gpt"] = Agent(name="gpt", team_name="team-a", pane_id="%2")

    team.set_peer("orch", "claude")
    assert team.peer_map == {"orch": "claude", "claude": "orch"}

    team.set_peer("orch", "gpt")
    assert team.peer_map == {"orch": "gpt", "gpt": "orch"}
    assert team.resolve_peer("claude") is None
    assert team.resolve_peer("gpt") == "orch"


def test_resolve_peer_prefers_anti_family_cli_among_no_peer_candidates(configure_hive_home, monkeypatch):
    configure_hive_home()
    from hive.agent import Agent

    team = Team(name="team-a", lead_pane_id="", tmux_window="dev:0")
    team.agents = {
        "alpha": Agent(name="alpha", team_name="team-a", pane_id="%1", cli="claude"),
        "bravo": Agent(name="bravo", team_name="team-a", pane_id="%2", cli="codex"),
        "charlie": Agent(name="charlie", team_name="team-a", pane_id="%3", cli="claude"),
    }

    # alpha (claude) wants anti=codex → bravo
    assert team.resolve_peer("alpha") == "bravo"
    # bravo (codex) wants anti=claude → alpha or charlie; sorted → alpha
    assert team.resolve_peer("bravo") == "alpha"
    # charlie (claude) wants anti=codex → bravo
    assert team.resolve_peer("charlie") == "bravo"


def test_resolve_peer_falls_back_to_any_no_peer_when_no_anti_family(configure_hive_home, monkeypatch):
    configure_hive_home()
    from hive.agent import Agent

    team = Team(name="team-a", lead_pane_id="", tmux_window="dev:0")
    team.agents = {
        "alpha": Agent(name="alpha", team_name="team-a", pane_id="%1", cli="claude"),
        "bravo": Agent(name="bravo", team_name="team-a", pane_id="%2", cli="claude"),
    }

    # alpha (claude) wants codex, none available → fall back to sorted candidate = bravo
    assert team.resolve_peer("alpha") == "bravo"
    assert team.resolve_peer("bravo") == "alpha"


def test_resolve_peer_skips_members_already_in_explicit_peer_map(configure_hive_home, monkeypatch):
    configure_hive_home()
    from hive.agent import Agent

    team = Team(
        name="team-a",
        lead_pane_id="",
        tmux_window="dev:0",
        peer_map={"bravo": "charlie", "charlie": "bravo"},
    )
    team.agents = {
        "alpha": Agent(name="alpha", team_name="team-a", pane_id="%1", cli="claude"),
        "bravo": Agent(name="bravo", team_name="team-a", pane_id="%2", cli="codex"),
        "charlie": Agent(name="charlie", team_name="team-a", pane_id="%3", cli="codex"),
    }

    # bravo / charlie bound explicitly
    assert team.resolve_peer("bravo") == "charlie"
    assert team.resolve_peer("charlie") == "bravo"
    # alpha's candidates filter out bravo & charlie (both in explicit peer_map) → None
    assert team.resolve_peer("alpha") is None


def test_team_clear_peer_only_removes_explicit_mapping(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.member_role_for_pane", lambda _pane_id: "agent")
    team = Team(name="team-a", lead_pane_id="%0", tmux_window="dev:0", peer_map={"orch": "claude", "claude": "orch"})
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1")
    team.agents["gpt"] = Agent(name="gpt", team_name="team-a", pane_id="%2")

    assert team.clear_peer("orch") == "claude"
    assert team.peer_map == {}
    assert team.peer_mode() == "none"


def test_team_spawn_tags_agent_and_passes_workflow_as_initial_skill(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%0")
    spawned = []
    tagged = []
    layouts = []
    sent = []

    agent = Agent(name="claude", team_name="team-a", pane_id="%9")
    monkeypatch.setattr(
        "hive.team.Agent.spawn",
        lambda **kwargs: spawned.append(kwargs) or agent,
    )
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *args, **kwargs: tagged.append(args))
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.team.tmux.enable_pane_border_status", lambda target: layouts.append(("border", target)))
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda _t: (200, 50))
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda _t: ["%1", "%9"])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda target, option, value: layouts.append((target, option, value)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda target, preset: layouts.append(("layout", target, preset)))
    monkeypatch.setattr("hive.agent.Agent.send", lambda self, text: sent.append(text))

    team = Team(name="team-a", lead_pane_id="%0")
    result = team.spawn("claude", workflow="code-review", prompt="start now")

    assert result is agent
    assert spawned[0]["target_pane"] == "%0"
    assert spawned[0]["skill"] == "code-review"
    assert spawned[0]["prompt"] == "start now"
    assert tagged == [("%9", "agent", "claude", "team-a")]
    assert sent == []
    assert ("border", "dev:1") in layouts


def test_team_spawn_portrait_window_applies_even_vertical(configure_hive_home, monkeypatch):
    """Guards Bug 1 regression: portrait window must end on `even-vertical`,
    not the legacy hardcoded `main-vertical`."""
    configure_hive_home(tmux_inside=True, current_pane="%0")
    spawned: list[dict] = []
    layouts: list[tuple] = []
    agent = Agent(name="claude", team_name="team-a", pane_id="%9")

    monkeypatch.setattr(
        "hive.team.Agent.spawn",
        lambda **kwargs: spawned.append(kwargs) or agent,
    )
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *args, **kwargs: None)
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: "dev:1")
    monkeypatch.setattr("hive.team.tmux.enable_pane_border_status", lambda target: None)
    monkeypatch.setattr("hive.team.tmux.list_panes", lambda _t: ["%0"])
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda _t: (191, 171))
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda _t: ["%0", "%9"])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda *a, **kw: layouts.append(("opt", a)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: layouts.append(("layout", t, p)))

    team = Team(name="team-a", lead_pane_id="%0")
    team.spawn("claude")

    assert ("layout", "dev:1", "even-vertical") in layouts
    # Portrait must not set main-pane-width.
    assert not any(call[0] == "opt" and call[1][1] == "main-pane-width" for call in layouts)
    # Pre-spawn split should also follow portrait orientation (vertical = False).
    assert spawned[0]["split_horizontal"] is False


def test_team_spawn_second_agent_splits_from_last_agent(configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=True, current_pane="%0")
    calls = []
    monkeypatch.setattr(
        "hive.team.Agent.spawn",
        lambda **kwargs: calls.append(kwargs) or Agent(name=kwargs["name"], team_name="team-a", pane_id=f"%{len(calls)+8}"),
    )
    monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *_args, **_kwargs: None)
    monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: None)

    team = Team(name="team-a", lead_pane_id="%0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%9")
    team.spawn("gpt")

    assert calls[0]["target_pane"] == "%9"
    assert calls[0]["split_horizontal"] is False
    assert calls[0]["skill"] == "hive"


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
    monkeypatch.setattr("hive.team.tmux.has_session", lambda name: name == "dev")
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda pane: pane != "%dead")
    monkeypatch.setattr(
        "hive.team.tmux.get_pane_current_command",
        lambda pane: {"%0": "python3.12", "%1": "droid", "%2": "zsh"}.get(pane, ""),
    )
    monkeypatch.setattr("hive.team.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.team.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.team.tmux.list_tty_processes", lambda _tty: [])
    team = Team(name="team-a", workspace="/tmp/ws", lead_pane_id="%0", lead_session_id="sess-1", tmux_session="dev")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1", model="m1")

    payload = team.status()

    assert payload["tmuxSession"] == "dev"
    assert payload["tmuxWindow"] == ""
    orch = next(member for member in payload["members"] if member["name"] == "orch")
    claude = next(member for member in payload["members"] if member["name"] == "claude")
    assert orch["role"] == "terminal"
    assert claude["role"] == "agent"
    assert team.is_tmux_alive() is True
    team.lead_pane_id = "%dead"
    assert team.is_tmux_alive() is False


def test_team_status_stays_local_only(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.get_pane_current_command", lambda pane: "droid" if pane == "%1" else "zsh")

    team = Team(name="team-a", lead_pane_id="%0")
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1")

    payload = team.status()

    orch = next(member for member in payload["members"] if member["name"] == "orch")
    claude = next(member for member in payload["members"] if member["name"] == "claude")
    assert "sessionId" not in orch
    assert "model" not in orch
    assert "alive" not in orch
    assert "sessionId" not in claude
    assert "model" not in claude
    assert "alive" not in claude


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

    team.shutdown("claude")
    team.shutdown()
    team.cleanup()

    assert calls[:3] == [("shutdown", "%1"), ("shutdown", "%1"), ("shutdown", "%2")]
    assert ("kill", "%1") in calls and ("kill", "%2") in calls
    assert ("clear", "%0") in calls


def test_find_team_window_prefers_pane_window_on_duplicate(configure_hive_home, monkeypatch):
    """When two windows claim the same team, the one containing prefer_pane wins.

    The losing duplicate here is stale (no live member panes), so it is cleared.
    """
    configure_hive_home()

    list_output = "dev:2\tmy-team\t/tmp/ws\tdesc\t0\ndev:3\tmy-team\t/tmp/ws\tdesc\t0\n"
    monkeypatch.setattr(
        "hive.team.tmux._run",
        lambda args, check=True: type("R", (), {"stdout": list_output, "returncode": 0})(),
    )
    monkeypatch.setattr("hive.team.tmux.get_pane_window_target", lambda pane: "dev:3" if pane == "%99" else None)
    # No live member panes anywhere → the losing window dev:2 is provably stale.
    monkeypatch.setattr("hive.team.tmux.list_panes_full", lambda target: [])
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
    # All duplicates are stale (no live member panes) → all non-kept get cleared.
    monkeypatch.setattr("hive.team.tmux.list_panes_full", lambda target: [])
    cleared: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append((wt, key)))

    _gc_stale_team_windows("my-team", keep="dev:3", all_windows=["dev:2", "dev:3", "dev:4"])

    stale_windows = {wt for wt, _ in cleared}
    assert stale_windows == {"dev:2", "dev:4"}
    assert ("dev:3", "@hive-team") not in cleared


def test_gc_stale_team_windows_skips_live_duplicate(configure_hive_home, monkeypatch):
    """A duplicate window with live member panes is never cleared (Bug A safety)."""
    configure_hive_home()
    from hive.tmux import PaneInfo

    def fake_list_panes(target):
        if target == "dev:2":
            return [PaneInfo("%40", "", "codex", role="agent", agent="validator", team="my-team")]
        return []

    monkeypatch.setattr("hive.team.tmux.list_panes_full", fake_list_panes)
    cleared: list[str] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append(wt))

    _gc_stale_team_windows("my-team", keep="dev:3", all_windows=["dev:2", "dev:3", "dev:4"])

    assert "dev:2" not in cleared  # live duplicate preserved
    assert "dev:4" in cleared      # stale duplicate still cleared


def test_find_team_window_keeps_live_duplicate(configure_hive_home, monkeypatch):
    """Two live windows share a team name; prefer_pane picks one for routing and
    the other keeps its tags. Bug A: never clobber a live duplicate."""
    configure_hive_home()

    list_output = "dev:2\t0-2\t/tmp/ws2\tdesc\t0\ndev:3\t0-2\t/tmp/ws3\tdesc\t0\n"
    monkeypatch.setattr(
        "hive.team.tmux._run",
        lambda args, check=True: type("R", (), {"stdout": list_output, "returncode": 0})(),
    )
    monkeypatch.setattr("hive.team.tmux.get_pane_window_target", lambda pane: "dev:3" if pane == "%40" else None)

    from hive.tmux import PaneInfo

    def fake_list_panes(target):
        if target == "dev:2":
            return [
                PaneInfo("%10", "", "claude", role="agent", agent="worker", team="0-2"),
                PaneInfo("%11", "", "codex", role="agent", agent="validator", team="0-2"),
            ]
        if target == "dev:3":
            return [
                PaneInfo("%40", "", "claude", role="agent", agent="worker", team="0-2"),
                PaneInfo("%41", "", "codex", role="agent", agent="validator", team="0-2"),
            ]
        return []

    monkeypatch.setattr("hive.team.tmux.list_panes_full", fake_list_panes)
    cleared: list[str] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append(wt))

    wt, _ = _find_team_window("0-2", prefer_pane="%40")

    assert wt == "dev:3"          # prefer_pane window wins for routing
    assert "dev:2" not in cleared  # the other live duplicate keeps its tags
