import json
from types import SimpleNamespace

import pytest

from hive.cli import cli


class _FakeAgent:
    def __init__(self) -> None:
        self.killed = False
        self.pane_id = "%99"

    def kill(self) -> None:
        self.killed = True


class _FakeTeam:
    def __init__(self, name: str, agents: dict[str, _FakeAgent], tmux_window: str = "dev:0") -> None:
        self.name = name
        self.agents = agents
        self.tmux_window = tmux_window

    def get(self, name: str) -> _FakeAgent:
        if name not in self.agents:
            raise KeyError(f"Agent '{name}' not found")
        return self.agents[name]


def _install_layout_mocks(monkeypatch, *, size: tuple[int, int] = (220, 60), pane_count: int = 2):
    calls: list[tuple] = []
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda _t: size)
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda _t: [f"%{i}" for i in range(pane_count)])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda t, k, v: calls.append(("opt", t, k, v)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: calls.append(("layout", t, p)))
    return calls


@pytest.mark.cli
def test_kill_qualified_crosses_team(runner, configure_hive_home, monkeypatch):
    """Qualified name routes to the target pane's team, not the caller's."""
    configure_hive_home()
    target_agent = _FakeAgent()
    scoped_team = _FakeTeam("scoped", {})  # caller's scoped team has no such agent
    peer_team = _FakeTeam("peer-1", {"squad.worker-1": target_agent})

    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: ("scoped", scoped_team),
    )
    monkeypatch.setattr(
        "hive.cli._find_qualified_agent_target",
        lambda qualified: ("peer-1", "squad.worker-1") if qualified == "squad.worker-1" else None,
    )
    monkeypatch.setattr("hive.cli._load_team", lambda name: peer_team)
    layout_calls = _install_layout_mocks(monkeypatch)

    result = runner.invoke(cli, ["kill", "squad.worker-1"])

    assert result.exit_code == 0, result.output
    assert target_agent.killed is True
    assert "squad.worker-1" not in peer_team.agents  # removed from target team
    # Kill must trigger adaptive rebalance against the target team's window.
    assert any(call[0] == "layout" and call[1] == "dev:0" for call in layout_calls)
    payload = json.loads(result.output)
    assert payload == {
        "member": "squad.worker-1",
        "action": "kill",
        "pane": "%99",
        "removedFromTeam": True,
        "success": True,
    }


@pytest.mark.cli
def test_kill_bare_name_uses_scoped_team(runner, configure_hive_home, monkeypatch):
    """Bare names keep the legacy behaviour: kill within the scoped team."""
    configure_hive_home()
    agent = _FakeAgent()
    scoped_team = _FakeTeam("team-x", {"opus": agent})

    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: ("team-x", scoped_team),
    )
    # Qualified lookup must not be triggered for bare names.
    monkeypatch.setattr(
        "hive.cli._find_qualified_agent_target",
        lambda qualified: pytest.fail("bare name should not hit qualified lookup"),
    )
    layout_calls = _install_layout_mocks(monkeypatch)

    result = runner.invoke(cli, ["kill", "opus"])

    assert result.exit_code == 0, result.output
    assert agent.killed is True
    assert "opus" not in scoped_team.agents
    assert any(call[0] == "layout" for call in layout_calls)
    payload = json.loads(result.output)
    assert payload == {
        "member": "opus",
        "action": "kill",
        "pane": "%99",
        "removedFromTeam": True,
        "success": True,
    }


@pytest.mark.cli
def test_kill_unknown_agent_errors(runner, configure_hive_home, monkeypatch):
    """Missing agent surfaces a clear error, no kill attempted."""
    configure_hive_home()
    scoped_team = _FakeTeam("team-x", {})

    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: ("team-x", scoped_team),
    )

    result = runner.invoke(cli, ["kill", "ghost"])

    assert result.exit_code != 0
    assert "ghost" in (result.output + (str(result.exception) if result.exception else ""))
