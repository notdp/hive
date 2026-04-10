from pathlib import Path

import pytest
from click.testing import CliRunner

from hive.tmux import PaneInfo


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


class FakeTmuxState:
    """In-memory tmux state for testing. Tracks window and pane options."""

    def __init__(self):
        self.window_options: dict[str, dict[str, str]] = {}
        self.pane_options: dict[str, dict[str, str]] = {}
        self.pane_alive: dict[str, bool] = {}

    def set_window_option(self, target: str, option: str, value: str) -> None:
        key = option.removeprefix("@")
        self.window_options.setdefault(target, {})[key] = value

    def get_window_option(self, target: str, key: str) -> str | None:
        return self.window_options.get(target, {}).get(key)

    def clear_window_option(self, target: str, option: str) -> None:
        key = option.removeprefix("@")
        self.window_options.get(target, {}).pop(key, None)

    def tag_pane(self, pane_id: str, role: str, agent: str, team: str,
                 *, model: str = "", cli: str = "", color: str = "") -> None:
        opts = {"hive-role": role, "hive-agent": agent, "hive-team": team}
        if model:
            opts["hive-model"] = model
        if cli:
            opts["hive-cli"] = cli
        if color:
            opts["hive-color"] = color
        self.pane_options[pane_id] = {**self.pane_options.get(pane_id, {}), **opts}

    def get_pane_option(self, pane_id: str, key: str) -> str | None:
        return self.pane_options.get(pane_id, {}).get(key)

    def clear_pane_tags(self, pane_id: str) -> None:
        self.pane_options.pop(pane_id, None)

    def find_team_window(self, name: str, *, prefer_pane: str = "") -> tuple[str, dict[str, str]]:
        for target, opts in self.window_options.items():
            if opts.get("hive-team") == name:
                return target, {
                    "workspace": opts.get("hive-workspace", ""),
                    "desc": opts.get("hive-desc", ""),
                    "created": opts.get("hive-created", "0"),
                }
        return "", {}

    def list_teams(self) -> list[dict[str, str]]:
        teams = []
        for target, opts in self.window_options.items():
            team_name = opts.get("hive-team")
            if team_name:
                teams.append({
                    "name": team_name,
                    "tmuxWindow": target,
                    "tmuxSession": target.split(":")[0] if ":" in target else "",
                    "workspace": opts.get("hive-workspace", ""),
                })
        return teams

    def list_panes_full(self, target: str) -> list[PaneInfo]:
        result = []
        for pane_id, opts in self.pane_options.items():
            if opts.get("hive-team") and target:
                result.append(PaneInfo(
                    pane_id=pane_id,
                    title="",
                    command="droid",
                    role=opts.get("hive-role", ""),
                    agent=opts.get("hive-agent", ""),
                    team=opts.get("hive-team", ""),
                    model=opts.get("hive-model", ""),
                    cli=opts.get("hive-cli", ""),
                    color=opts.get("hive-color", ""),
                ))
        return result


@pytest.fixture
def configure_hive_home(monkeypatch, tmp_path):
    def _configure(*, tmux_inside: bool = True, current_pane: str = "%0", session_name: str = "dev"):
        hive_home = tmp_path / ".hive"
        factory_home = tmp_path / ".factory"
        codex_home = tmp_path / ".codex"
        claude_home = tmp_path / ".claude"
        monkeypatch.setenv("HIVE_HOME", str(hive_home))
        monkeypatch.setenv("FACTORY_HOME", str(factory_home))
        monkeypatch.setenv("CODEX_HOME", str(codex_home))
        monkeypatch.setenv("CLAUDE_HOME", str(claude_home))
        monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / ".cache"))
        monkeypatch.setattr("hive.team.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": None)
        monkeypatch.setattr("hive.cli.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.context.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.context.CONTEXT_DIR", hive_home / "contexts")
        monkeypatch.setattr("hive.context.CURRENT_CONTEXT_FILE", hive_home / "current.json")

        state = FakeTmuxState()

        # team.tmux mocks
        monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: tmux_inside)
        monkeypatch.setattr("hive.team.tmux.get_current_pane_id", lambda: current_pane)
        monkeypatch.setattr("hive.team.tmux.get_current_session_name", lambda: session_name)
        monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: f"{session_name}:0")
        monkeypatch.setattr("hive.team.tmux.get_pane_current_command", lambda _pane: "droid")
        monkeypatch.setattr("hive.team.tmux.has_session", lambda _name: True)
        monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
        monkeypatch.setattr("hive.team.tmux.tag_pane", state.tag_pane)
        monkeypatch.setattr("hive.team.tmux.clear_pane_tags", state.clear_pane_tags)
        monkeypatch.setattr("hive.team.tmux.set_window_option", state.set_window_option)
        monkeypatch.setattr("hive.team.tmux.get_window_option", state.get_window_option)
        monkeypatch.setattr("hive.team.tmux.clear_window_option", state.clear_window_option)
        monkeypatch.setattr("hive.team.tmux.list_panes_full", state.list_panes_full)
        monkeypatch.setattr("hive.team._find_team_window", state.find_team_window)
        monkeypatch.setattr("hive.team.list_teams", state.list_teams)

        # cli.tmux mocks
        monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: tmux_inside)
        monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: current_pane)
        monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: session_name)
        monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: f"{session_name}:0")
        monkeypatch.setattr("hive.cli.tmux.get_pane_current_command", lambda _pane: "droid")
        monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: f"{session_name}:0")
        monkeypatch.setattr("hive.cli.tmux.get_pane_option", state.get_pane_option)
        monkeypatch.setattr("hive.cli.tmux.get_window_option", state.get_window_option)
        monkeypatch.setattr("hive.cli.tmux.set_window_option", state.set_window_option)
        monkeypatch.setattr("hive.cli.tmux.clear_window_option", state.clear_window_option)
        monkeypatch.setattr("hive.cli.tmux.is_pane_alive", lambda _pane: True)
        monkeypatch.setattr("hive.cli.tmux.tag_pane", state.tag_pane)
        monkeypatch.setattr("hive.cli.tmux.clear_pane_tags", state.clear_pane_tags)
        monkeypatch.delenv("TMUX_PANE", raising=False)
        return hive_home

    return _configure


@pytest.fixture
def mock_tmux_send(monkeypatch):
    sent: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.agent.tmux.send_keys", lambda pane, text, enter=True: sent.append((pane, text)))
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda pane, key: sent.append((pane, f"<{key}>")))
    monkeypatch.setattr("hive.agent.time.sleep", lambda _s: None)
    return sent


def pytest_collection_modifyitems(items):
    tests_root = Path(__file__).resolve().parent
    for item in items:
        rel = Path(str(item.path)).resolve().relative_to(tests_root)
        if not rel.parts:
            continue
        top_level = rel.parts[0]
        if top_level in {"unit", "cli", "e2e"}:
            item.add_marker(getattr(pytest.mark, top_level))
