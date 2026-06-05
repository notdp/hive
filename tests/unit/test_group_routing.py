import pytest

from hive import cli as cli_module
from hive.tmux import PaneInfo


def _pane(agent: str, team: str, group: str, pane_id: str = "%1") -> PaneInfo:
    return PaneInfo(
        pane_id=pane_id,
        title="",
        command="",
        role="agent",
        agent=agent,
        team=team,
        cli="",
        group=group,
    )


def test_find_qualified_returns_none_for_bare_name():
    assert cli_module._find_qualified_agent_target("orch") is None


def test_find_qualified_finds_unique_match(monkeypatch):
    panes = [
        _pane("crew.worker-1", "peer-1", "crew", "%1"),
        _pane("crew.judge-1", "peer-1", "crew", "%2"),
        _pane("other", "peer-1", "", "%3"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("crew.worker-1") == (
        "peer-1",
        "crew.worker-1",
    )


def test_find_qualified_supports_public_crew_name_namespace(monkeypatch):
    panes = [
        _pane("peaky.worker-1000", "dev-0-cell-1000", "peaky", "%1"),
        _pane("shelby.worker-1000", "dev-1-cell-1000", "shelby", "%2"),
        _pane("peaky.orch", "dev-0", "peaky", "%3"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("peaky.worker-1000") == (
        "dev-0-cell-1000",
        "peaky.worker-1000",
    )


def test_find_qualified_returns_none_when_agent_missing(monkeypatch):
    panes = [_pane("crew.worker-1", "peer-1", "crew", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("crew.worker-2") is None


def test_find_qualified_raises_on_ambiguous(monkeypatch):
    panes = [
        _pane("crew.worker-1", "peer-1", "crew", "%1"),
        _pane("crew.worker-1", "peer-2", "crew", "%5"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    with pytest.raises(ValueError, match="unique"):
        cli_module._find_qualified_agent_target("crew.worker-1")


def test_find_qualified_ignores_mismatched_group(monkeypatch):
    panes = [_pane("crew.worker-1", "peer-1", "mafia", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("crew.worker-1") is None


def test_find_qualified_ignores_same_suffix_in_other_public_crew(monkeypatch):
    panes = [
        _pane("shelby.worker-1000", "dev-1-cell-1000", "shelby", "%2"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("peaky.worker-1000") is None


def test_find_qualified_requires_non_empty_group_prefix():
    assert cli_module._find_qualified_agent_target(".worker-1") is None


def test_resolve_send_target_team_loads_target_team_for_qualified_name(monkeypatch):
    """Qualified `crew.x` routing bypasses current team and loads target's team."""
    from types import SimpleNamespace

    panes = [_pane("crew.worker-1", "peer-1", "crew", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    loaded: list[str] = []

    def fake_load(name: str):
        loaded.append(name)
        return SimpleNamespace(name=name)

    monkeypatch.setattr("hive.cli._load_team", fake_load)

    team_name, t = cli_module._resolve_send_target_team("crew.worker-1")

    assert team_name == "peer-1"
    assert loaded == ["peer-1"]
    assert t.name == "peer-1"


def test_resolve_send_target_team_loads_target_team_for_public_crew_name(monkeypatch):
    """Qualified public crew names should resolve exactly like legacy `crew.x`."""
    from types import SimpleNamespace

    panes = [_pane("peaky.worker-1000", "dev-0-cell-1000", "peaky", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    loaded: list[str] = []

    def fake_load(name: str):
        loaded.append(name)
        return SimpleNamespace(name=name)

    monkeypatch.setattr("hive.cli._load_team", fake_load)

    team_name, t = cli_module._resolve_send_target_team("peaky.worker-1000")

    assert team_name == "dev-0-cell-1000"
    assert loaded == ["dev-0-cell-1000"]
    assert t.name == "dev-0-cell-1000"
