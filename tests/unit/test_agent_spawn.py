"""Tests for Agent.spawn model/skill/env handling."""

from hive.agent import (
    Agent,
    _cleanup_runtime_settings_override,
    _detect_new_session,
    _write_runtime_settings_override,
    detect_current_session_id,
)
import json


def _setup_tmux_mocks(monkeypatch):
    calls: list[str] = []

    monkeypatch.setattr("hive.agent.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.agent.tmux.set_pane_title", lambda *_: None)
    monkeypatch.setattr("hive.agent.tmux.set_pane_border_color", lambda *_: None)
    monkeypatch.setattr("hive.agent.tmux.wait_for_text", lambda *_args, **_kw: True)
    monkeypatch.setattr("hive.agent.tmux.send_keys", lambda _pane, text: calls.append(text))
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)

    return calls


def test_spawn_loads_specified_skill(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="", cwd="/tmp", is_first=True,
        skill="cross-review",
    )

    assert "/skill cross-review" in calls
    # Should NOT send hive bootstrap message
    assert not any("hive teammate" in c for c in calls)


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


def test_spawn_hive_bootstraps_and_sends_prompt(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="hive",
        prompt="Please check your inbox.",
    )

    assert "/skill hive" in calls
    assert any("Use `hive current`, `hive who`, `hive send`, and `hive status-set`" in c for c in calls)
    assert any("<HIVE ...> ... </HIVE>" in c for c in calls)
    assert "Please check your inbox." in calls


def test_load_skill_sends_slash_command(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)
    agent = Agent(name="w1", team_name="t", pane_id="%0")

    agent.load_skill("cross-review")

    assert calls == ["/skill cross-review"]


def test_spawn_uses_runtime_settings_override(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)
    cleaned: list[str] = []

    monkeypatch.setattr(
        "hive.agent._write_runtime_settings_override",
        lambda _model: (__import__("pathlib").Path("/tmp/hive-runtime-settings.json"), "custom:Claude-Opus-4.6-0"),
    )
    monkeypatch.setattr(
        "hive.agent._cleanup_runtime_settings_override",
        lambda path: cleaned.append(str(path) if path else ""),
    )

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="custom:claude-opus-4-6", cwd="/tmp", is_first=True,
        skill="none",
    )

    startup_cmd = calls[0]
    assert "--settings '/tmp/hive-runtime-settings.json'" in startup_cmd
    assert cleaned == ["/tmp/hive-runtime-settings.json"]


def test_write_runtime_settings_override_resolves_custom_model(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {"model": "opus"},
        "customModels": [
            {"model": "claude-opus-4-6", "displayName": "Claude Opus 4.6", "id": "custom:Claude-Opus-4.6-0"}
        ],
    }))
    monkeypatch.setattr("hive.agent.SETTINGS_FILE", settings_file)

    runtime_path, resolved = _write_runtime_settings_override("custom:claude-opus-4-6")
    assert resolved == "custom:Claude-Opus-4.6-0"
    assert runtime_path is not None

    data = json.loads(runtime_path.read_text())
    assert data == {"sessionDefaultSettings": {"model": "custom:Claude-Opus-4.6-0"}}

    original = json.loads(settings_file.read_text())
    assert original["sessionDefaultSettings"]["model"] == "opus"

    _cleanup_runtime_settings_override(runtime_path)
    assert not runtime_path.exists()


def test_write_runtime_settings_override_keeps_direct_model(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {"model": "custom:my-model"},
    }))
    monkeypatch.setattr("hive.agent.SETTINGS_FILE", settings_file)

    runtime_path, resolved = _write_runtime_settings_override("custom:my-model")
    assert resolved == "custom:my-model"
    assert runtime_path is not None
    data = json.loads(runtime_path.read_text())
    assert data == {"sessionDefaultSettings": {"model": "custom:my-model"}}
    _cleanup_runtime_settings_override(runtime_path)


def test_write_runtime_settings_override_returns_none_when_model_empty():
    runtime_path, resolved = _write_runtime_settings_override("")
    assert runtime_path is None
    assert resolved == ""


def test_detect_new_session_matches_resolved_model_id(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    project_dir = sessions_dir / "-tmp-test"
    project_dir.mkdir(parents=True)
    monkeypatch.setattr("hive.agent.SESSIONS_DIR", sessions_dir)

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


def test_detect_current_session_prefers_newest_session(monkeypatch):
    monkeypatch.setattr("hive.agent._list_sessions", lambda _cwd: {"old", "new"})
    monkeypatch.setattr("hive.agent._session_timestamp", lambda _cwd, sid: {"old": 1, "new": 2}[sid])

    assert detect_current_session_id("/tmp/test") == "new"


def test_detect_current_session_prefers_matching_model(monkeypatch):
    monkeypatch.setattr("hive.agent._list_sessions", lambda _cwd: {"a", "b"})
    monkeypatch.setattr("hive.agent._session_timestamp", lambda _cwd, sid: {"a": 1, "b": 2}[sid])
    monkeypatch.setattr(
        "hive.agent._read_session_model",
        lambda _cwd, sid: {"a": "custom:Claude-Opus-4.6-0", "b": "custom:GPT-5.4-1"}[sid],
    )

    assert detect_current_session_id("/tmp/test", model="custom:Claude-Opus-4.6-0") == "a"
