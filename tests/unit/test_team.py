import pytest

from hive import tmux as _tmux
from hive.agent import Agent
from hive.team import Team, _find_team_window, _gc_stale_team_windows, duplicate_team_bindings


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


@pytest.mark.parametrize("name", ["ccd", "ccd.desk", "a.b"])
def test_team_create_rejects_reserved_or_dotted_names(configure_hive_home, name):
    # `hive send` parses `<team>.<member>` / `ccd.<session>`: a team named
    # ccd, or one carrying a dot, would be unaddressable
    configure_hive_home(tmux_inside=True)

    with pytest.raises(ValueError, match="invalid"):
        Team.create(name)


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
    )
    team.agents["claude"] = Agent(name="claude", team_name="team-a", pane_id="%1", model="m1", cwd="/tmp")

    team.save()
    assert borders == ["dev:0"]

    # Set up pane tags for load to find (in real usage, set during create/spawn)
    _tmux.tag_pane("%0", "agent", "orch", "team-a", cli="claude")
    _tmux.tag_pane("%1", "agent", "claude", "team-a", cli="claude")

    loaded = Team.load("team-a")

    assert loaded.name == "team-a"
    assert loaded.description == "demo"
    assert loaded.tmux_window == "dev:0"
    assert loaded.tmux_window_id == "@0"
    assert loaded.agents["orch"].pane_id == "%0"
    assert loaded.agents["claude"].pane_id == "%1"


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


def test_team_spawn_tags_agent_and_passes_skill(configure_hive_home, monkeypatch):
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
    result = team.spawn("claude", skill="demo-review", prompt="start now")

    assert result is agent
    assert spawned[0]["target_pane"] == "%0"
    assert spawned[0]["skill"] == "demo-review"
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


def test_team_get_resolves_lead_and_members(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
    alive = Agent(name="claude", team_name="team-a", pane_id="%1")

    team = Team(name="team-a", lead_pane_id="%0")
    team.agents = {"claude": alive}

    assert team.get("orch").pane_id == "%0"
    assert team.get("claude") is alive


def test_team_status_and_is_tmux_alive(configure_hive_home, monkeypatch):
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.has_session", lambda name: name == "dev")
    monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda pane: pane != "%dead")
    monkeypatch.setattr(
        "hive.team.tmux.get_pane_current_command",
        lambda pane: {"%0": "python3.12", "%1": "codex", "%2": "zsh"}.get(pane, ""),
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
    monkeypatch.setattr("hive.team.tmux.get_pane_current_command", lambda pane: "codex" if pane == "%1" else "zsh")

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
    monkeypatch.setattr("hive.team.tmux.list_panes_full_or_none", lambda target: [])
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
            return [PaneInfo("%50", "", "codex", role="agent", agent="rev-a", team="my-team")]
        return [PaneInfo("%40", "", "codex", role="", agent="", team="")]

    monkeypatch.setattr("hive.team.tmux.list_panes_full_or_none", fake_list_panes)
    cleared: list[str] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append(wt))

    wt, _ = _find_team_window("my-team")

    assert wt == "dev:3"
    assert "dev:2" in cleared


def test_gc_stale_team_windows_clears_non_kept(configure_hive_home, monkeypatch):
    configure_hive_home()
    # All duplicates are stale (no live member panes) → all non-kept get cleared.
    monkeypatch.setattr("hive.team.tmux.list_panes_full_or_none", lambda target: [])
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

    monkeypatch.setattr("hive.team.tmux.list_panes_full_or_none", fake_list_panes)
    cleared: list[str] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append(wt))

    _gc_stale_team_windows("my-team", keep="dev:3", all_windows=["dev:2", "dev:3", "dev:4"])

    assert "dev:2" not in cleared  # live duplicate preserved
    assert "dev:4" in cleared      # stale duplicate still cleared


def test_gc_stale_team_windows_skips_cleanup_on_tmux_failure(configure_hive_home, monkeypatch):
    """A failed pane listing is unknown, not proof of staleness — clear nothing."""
    configure_hive_home()
    monkeypatch.setattr("hive.team.tmux.list_panes_full_or_none", lambda target: None)
    cleared: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append((wt, key)))

    _gc_stale_team_windows("my-team", keep="dev:3", all_windows=["dev:2", "dev:3", "dev:4"])

    assert cleared == []


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

    monkeypatch.setattr("hive.team.tmux.list_panes_full_or_none", fake_list_panes)
    cleared: list[str] = []
    monkeypatch.setattr("hive.team.tmux.clear_window_option", lambda wt, key: cleared.append(wt))

    wt, _ = _find_team_window("0-2", prefer_pane="%40")

    assert wt == "dev:3"          # prefer_pane window wins for routing
    assert "dev:2" not in cleared  # the other live duplicate keeps its tags


def test_duplicate_team_bindings_reports_only_collisions(configure_hive_home, monkeypatch):
    """Two windows sharing a team name are reported with their ids + live members;
    a uniquely-named team is not."""
    configure_hive_home()
    list_output = (
        "0:2\t@2\t0-2\t/tmp/hive-0-w2\n"
        "0:3\t@3\t0-2\t/tmp/hive-0-w3\n"
        "0:5\t@5\tsolo\t/tmp/hive-0-w5\n"
    )
    monkeypatch.setattr(
        "hive.team.tmux._run",
        lambda args, check=True: type("R", (), {"stdout": list_output, "returncode": 0})(),
    )

    from hive.tmux import PaneInfo

    def fake_list_panes(target):
        return {
            "0:2": [PaneInfo("%42", "", "claude", role="agent", agent="worker", team="0-2")],
            "0:3": [PaneInfo("%10", "", "claude", role="agent", agent="worker", team="0-2")],
            "0:5": [PaneInfo("%80", "", "claude", role="agent", agent="worker", team="solo")],
        }.get(target, [])

    monkeypatch.setattr("hive.team.tmux.list_panes_full", fake_list_panes)

    dupes = duplicate_team_bindings()

    assert len(dupes) == 1  # only the colliding team, not the unique "solo"
    assert dupes[0]["team"] == "0-2"
    windows = dupes[0]["windows"]
    assert {w["windowId"] for w in windows} == {"@2", "@3"}
    assert windows[0]["liveMembers"][0]["name"] == "worker"
    assert "manual" in dupes[0]["repair"]
