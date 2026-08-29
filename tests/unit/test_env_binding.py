"""Member identity from engine env (HIVE_TEAM / HIVE_MEMBER) — the pane-free
fallback lane of binding discovery."""
import pytest

from hive import cli as cli_mod

pytestmark = pytest.mark.unit


@pytest.fixture
def hive_home(monkeypatch, tmp_path):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / ".hive"))
    return tmp_path


def test_env_binding_resolves_identity_and_workspace(hive_home, monkeypatch):
    from hive import resume

    assert resume.record_team(
        handle="honey", workspace="/tmp/ws-h", created_at="1.0", now="t0",
        members=[{"name": "rex", "cli": "grok"}],
    ) == "written"
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setenv("HIVE_TEAM", "honey")
    monkeypatch.setenv("HIVE_MEMBER", "rex")

    binding = cli_mod._discover_tmux_binding()

    assert binding["team"] == "honey"
    assert binding["agent"] == "rex"
    assert binding["workspace"] == "/tmp/ws-h"
    assert binding["pane"] == ""


def test_env_binding_needs_both_markers(hive_home, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setenv("HIVE_TEAM", "honey")
    assert cli_mod._discover_tmux_binding() == {}


def test_pane_binding_beats_env(hive_home, monkeypatch):
    """A real pane binding wins: env identity is the fallback, not an override."""
    monkeypatch.setenv("HIVE_TEAM", "envteam")
    monkeypatch.setenv("HIVE_MEMBER", "envagent")
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%5")
    options = {
        ("%5", "hive-team"): "paneteam",
        ("%5", "hive-agent"): "paneagent",
        ("%5", "hive-role"): "agent",
        ("%5", "hive-group"): "",
    }
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option", lambda pane, key: options.get((pane, key))
    )
    monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: "dev:0")
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.get_window_option", lambda w, k: "/ws")

    binding = cli_mod._discover_tmux_binding()

    assert binding["team"] == "paneteam"
    assert binding["agent"] == "paneagent"
