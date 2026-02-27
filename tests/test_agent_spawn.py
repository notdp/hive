"""Tests for Agent.spawn model/skill/env handling."""

from unittest.mock import patch
from mission.agent import Agent, _set_model_in_settings, _restore_model_in_settings
import json
import tempfile
from pathlib import Path


def _setup_tmux_mocks(monkeypatch):
    calls: list[str] = []

    monkeypatch.setattr("mission.agent.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("mission.agent.tmux.set_pane_title", lambda *_: None)
    monkeypatch.setattr("mission.agent.tmux.set_pane_border_color", lambda *_: None)
    monkeypatch.setattr("mission.agent.tmux.wait_for_text", lambda *_args, **_kw: True)
    monkeypatch.setattr("mission.agent.tmux.send_keys", lambda _pane, text: calls.append(text))
    monkeypatch.setattr("mission.agent.time.sleep", lambda *_: None)

    return calls


def test_spawn_loads_specified_skill(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="", cwd="/tmp", is_first=True,
        skill="cross-review",
    )

    assert "/skill cross-review" in calls
    # Should NOT send mission bootstrap message
    assert not any("mission teammate" in c for c in calls)


def test_spawn_skips_skill_when_none(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    assert not any(c.startswith("/skill") for c in calls)


def test_spawn_passes_extra_env(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
        extra_env={"CR_WORKSPACE": "/tmp/cr-test"},
    )

    startup_cmd = calls[0]
    assert "CR_WORKSPACE=" in startup_cmd
    assert "/tmp/cr-test" in startup_cmd


def test_set_model_in_settings(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {"model": "opus"},
        "customModels": [
            {"model": "claude-opus-4-6", "displayName": "Claude Opus 4.6", "id": "custom:Claude-Opus-4.6-0"}
        ],
    }))
    monkeypatch.setattr("mission.agent.SETTINGS_FILE", settings_file)

    prev = _set_model_in_settings("custom:claude-opus-4-6")
    assert prev == "opus"

    data = json.loads(settings_file.read_text())
    assert data["sessionDefaultSettings"]["model"] == "custom:Claude-Opus-4.6-0"

    _restore_model_in_settings(prev)
    data = json.loads(settings_file.read_text())
    assert data["sessionDefaultSettings"]["model"] == "opus"


def test_set_model_noop_when_already_correct(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {"model": "custom:my-model"},
    }))
    monkeypatch.setattr("mission.agent.SETTINGS_FILE", settings_file)

    mtime_before = settings_file.stat().st_mtime_ns
    prev = _set_model_in_settings("custom:my-model")
    mtime_after = settings_file.stat().st_mtime_ns

    assert prev == "custom:my-model"
    assert mtime_before == mtime_after  # file not rewritten
