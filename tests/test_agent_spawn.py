"""Tests for Agent.spawn model/skill/env handling."""

from mission.agent import (
    Agent,
    _detect_new_session,
    _restore_model_in_settings,
    _set_model_in_settings,
)
import json


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

    had_model_key, prev, resolved = _set_model_in_settings("custom:claude-opus-4-6")
    assert had_model_key is True
    assert prev == "opus"
    assert resolved == "custom:Claude-Opus-4.6-0"

    data = json.loads(settings_file.read_text())
    assert data["sessionDefaultSettings"]["model"] == "custom:Claude-Opus-4.6-0"

    _restore_model_in_settings(had_model_key, prev)
    data = json.loads(settings_file.read_text())
    assert data["sessionDefaultSettings"]["model"] == "opus"


def test_set_model_noop_when_already_correct(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {"model": "custom:my-model"},
    }))
    monkeypatch.setattr("mission.agent.SETTINGS_FILE", settings_file)

    mtime_before = settings_file.stat().st_mtime_ns
    had_model_key, prev, resolved = _set_model_in_settings("custom:my-model")
    mtime_after = settings_file.stat().st_mtime_ns

    assert had_model_key is True
    assert prev == "custom:my-model"
    assert resolved == "custom:my-model"
    assert mtime_before == mtime_after  # file not rewritten


def test_restore_model_removes_temp_model_when_no_previous_key(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {},
    }))
    monkeypatch.setattr("mission.agent.SETTINGS_FILE", settings_file)

    had_model_key, prev, _ = _set_model_in_settings("custom:temp-model")
    assert had_model_key is False
    assert prev is None

    _restore_model_in_settings(had_model_key, prev)
    data = json.loads(settings_file.read_text())
    assert "model" not in data["sessionDefaultSettings"]


def test_detect_new_session_matches_resolved_model_id(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    project_dir = sessions_dir / "-tmp-test"
    project_dir.mkdir(parents=True)
    monkeypatch.setattr("mission.agent.SESSIONS_DIR", sessions_dir)

    old_sid = "11111111-1111-1111-1111-111111111111"
    old_path = project_dir / f"{old_sid}.settings.json"
    old_path.write_text(json.dumps({"model": "custom:Other-1"}))

    before = {old_sid}

    sid_a = "22222222-2222-2222-2222-222222222222"
    sid_b = "33333333-3333-3333-3333-333333333333"
    (project_dir / f"{sid_a}.settings.json").write_text(json.dumps({"model": "custom:Claude-Opus-4.6-0"}))
    (project_dir / f"{sid_b}.settings.json").write_text(json.dumps({"model": "custom:GPT-5.3-Codex-1"}))

    detected = _detect_new_session("/tmp/test", before, model="custom:Claude-Opus-4.6-0")
    assert detected == sid_a
