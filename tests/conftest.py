from pathlib import Path

import pytest
from click.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def configure_hive_home(monkeypatch, tmp_path):
    def _configure(*, tmux_inside: bool = True, current_pane: str = "%0", session_name: str = "dev"):
        hive_home = tmp_path / ".hive"
        monkeypatch.setattr("hive.team.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.team.detect_current_session_id", lambda _cwd: None)
        monkeypatch.setattr("hive.cli.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.context.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.context.CONTEXT_DIR", hive_home / "contexts")
        monkeypatch.setattr("hive.context.CURRENT_CONTEXT_FILE", hive_home / "current.json")
        monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: tmux_inside)
        monkeypatch.setattr("hive.team.tmux.get_current_pane_id", lambda: current_pane)
        monkeypatch.setattr("hive.team.tmux.has_session", lambda _name: True)
        monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
        monkeypatch.setattr("hive.team.tmux.tag_pane", lambda *_a, **_kw: None)
        monkeypatch.setattr("hive.team.tmux.clear_pane_tags", lambda *_a: None)
        monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: tmux_inside)
        monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: current_pane)
        monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: session_name)
        monkeypatch.setattr("hive.cli.tmux.is_pane_alive", lambda _pane: True)
        monkeypatch.setattr("hive.cli.tmux.tag_pane", lambda *_a, **_kw: None)
        monkeypatch.setattr("hive.cli.tmux.clear_pane_tags", lambda *_a: None)
        monkeypatch.delenv("TMUX_PANE", raising=False)
        return hive_home

    return _configure


@pytest.fixture
def mock_tmux_send(monkeypatch):
    sent: list[tuple[str, str]] = []
    monkeypatch.setattr("hive.agent.tmux.send_keys", lambda pane, text: sent.append((pane, text)))
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
