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
        _pane("kraken.worker-1", "peer-1", "kraken", "%1"),
        _pane("kraken.judge-1", "peer-1", "kraken", "%2"),
        _pane("other", "peer-1", "", "%3"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("kraken.worker-1") == (
        "peer-1",
        "kraken.worker-1",
    )


def test_find_qualified_supports_public_squad_name_namespace(monkeypatch):
    panes = [
        _pane("peaky.worker-1000", "dev-0-duo-1000", "peaky", "%1"),
        _pane("shelby.worker-1000", "dev-1-duo-1000", "shelby", "%2"),
        _pane("peaky.orch", "dev-0", "peaky", "%3"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("peaky.worker-1000") == (
        "dev-0-duo-1000",
        "peaky.worker-1000",
    )


def test_find_qualified_returns_none_when_agent_missing(monkeypatch):
    panes = [_pane("kraken.worker-1", "peer-1", "kraken", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("kraken.worker-2") is None


def test_find_qualified_raises_on_ambiguous(monkeypatch):
    panes = [
        _pane("kraken.worker-1", "peer-1", "kraken", "%1"),
        _pane("kraken.worker-1", "peer-2", "kraken", "%5"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    with pytest.raises(ValueError, match="unique"):
        cli_module._find_qualified_agent_target("kraken.worker-1")


def test_find_qualified_resolves_missing_group(monkeypatch):
    """A pane with matching @hive-agent but no @hive-group is still routable."""
    panes = [_pane("kraken.worker-1", "peer-1", "", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("kraken.worker-1") == (
        "peer-1",
        "kraken.worker-1",
    )


def test_find_qualified_rejects_conflicting_group(monkeypatch):
    """A pane with @hive-agent=kraken.worker-1 but @hive-group=mafia is a
    tagging mistake — the resolver must raise, not silently route."""
    panes = [_pane("kraken.worker-1", "peer-1", "mafia", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    with pytest.raises(ValueError, match="conflicting"):
        cli_module._find_qualified_agent_target("kraken.worker-1")


def test_find_qualified_ignores_same_suffix_in_other_public_squad(monkeypatch):
    panes = [
        _pane("shelby.worker-1000", "dev-1-duo-1000", "shelby", "%2"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("peaky.worker-1000") is None


def test_find_qualified_requires_non_empty_group_prefix():
    assert cli_module._find_qualified_agent_target(".worker-1") is None


def test_find_qualified_ambiguous_with_missing_groups(monkeypatch):
    """Duplicate @hive-agent across panes is ambiguous even when both lack group."""
    panes = [
        _pane("kraken.worker-1", "peer-1", "", "%1"),
        _pane("kraken.worker-1", "peer-2", "", "%5"),
    ]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    with pytest.raises(ValueError, match="unique"):
        cli_module._find_qualified_agent_target("kraken.worker-1")


def test_find_qualified_skips_pane_without_team(monkeypatch):
    """A pane with matching agent name but empty team is not a valid target."""
    panes = [_pane("kraken.worker-1", "", "kraken", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    assert cli_module._find_qualified_agent_target("kraken.worker-1") is None


def test_resolve_send_target_team_loads_target_team_for_qualified_name(monkeypatch):
    """Qualified `squad.x` routing bypasses current team and loads target's team."""
    from types import SimpleNamespace

    panes = [_pane("kraken.worker-1", "peer-1", "kraken", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    loaded: list[str] = []

    def fake_load(name: str):
        loaded.append(name)
        return SimpleNamespace(name=name)

    monkeypatch.setattr("hive.cli._load_team", fake_load)

    team_name, t = cli_module._resolve_send_target_team("kraken.worker-1")

    assert team_name == "peer-1"
    assert loaded == ["peer-1"]
    assert t.name == "peer-1"


def test_resolve_send_target_team_loads_target_team_for_public_squad_name(monkeypatch):
    """Qualified public squad names should resolve exactly like legacy `squad.x`."""
    from types import SimpleNamespace

    panes = [_pane("peaky.worker-1000", "dev-0-duo-1000", "peaky", "%1")]
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: panes)

    loaded: list[str] = []

    def fake_load(name: str):
        loaded.append(name)
        return SimpleNamespace(name=name)

    monkeypatch.setattr("hive.cli._load_team", fake_load)

    team_name, t = cli_module._resolve_send_target_team("peaky.worker-1000")

    assert team_name == "dev-0-duo-1000"
    assert loaded == ["dev-0-duo-1000"]
    assert t.name == "dev-0-duo-1000"
