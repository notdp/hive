"""Tests for Agent.spawn model/skill/env handling."""

from hive.agent import (
    Agent,
    _build_droid_model_settings,
    _detect_new_session,
    detect_current_session_id,
)
import json


def _setup_tmux_mocks(monkeypatch):
    calls: list[str] = []

    monkeypatch.setattr("hive.agent.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.agent.tmux.split_window", lambda target, horizontal=True, size=None, cwd=None: target)
    monkeypatch.setattr("hive.agent.tmux.get_pane_tty", lambda _pane: None)
    monkeypatch.setattr("hive.agent.tmux.set_pane_title", lambda *_: None)
    monkeypatch.setattr("hive.agent.tmux.set_pane_border_color", lambda *_: None)
    monkeypatch.setattr("hive.agent.tmux.wait_for_text", lambda *_args, **_kw: True)
    monkeypatch.setattr("hive.agent.tmux.send_keys", lambda _pane, text: calls.append(text))
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)

    return calls


def test_spawn_rejects_outside_tmux(monkeypatch):
    monkeypatch.setattr("hive.agent.tmux.is_inside_tmux", lambda: False)

    try:
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none")
    except ValueError as exc:
        assert "requires tmux" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_spawn_loads_specified_skill(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="", cwd="/tmp", is_first=True,
        skill="code-review",
    )

    assert "/code-review" in calls
    # Should NOT send hive bootstrap message
    assert not any("hive teammate" in c for c in calls)


def test_spawn_skips_skill_when_none(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    assert not any(c.startswith("/") and not c.startswith("/tmp") for c in calls)


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
    assert "HIVE_TEAM_NAME=" not in startup_cmd
    assert "HIVE_AGENT_NAME=" not in startup_cmd


def test_spawn_without_extra_env_does_not_export_default_hive_vars(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    startup_cmd = calls[0]
    assert "HIVE_TEAM_NAME=" not in startup_cmd
    assert "HIVE_AGENT_NAME=" not in startup_cmd
    assert "export " not in startup_cmd


def test_spawn_hive_bootstraps_and_sends_prompt(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="hive",
        prompt="Please check your inbox.",
    )

    assert "/hive" in calls
    assert any("Use `hive team`, `hive send`, and `hive status-set`" in c for c in calls)
    assert any("<HIVE ...> ... </HIVE>" in c for c in calls)
    assert "Please check your inbox." in calls


def test_load_skill_sends_slash_command(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)
    agent = Agent(name="w1", team_name="t", pane_id="%0")

    agent.load_skill("code-review")

    assert calls == ["/code-review"]


def test_spawn_droid_uses_process_substitution_for_model(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    monkeypatch.setattr(
        "hive.agent._build_droid_model_settings",
        lambda _model: ('{"sessionDefaultSettings":{"model":"custom:Claude-Opus-4.6-0"}}', "custom:Claude-Opus-4.6-0"),
    )

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="custom:claude-opus-4-6", cwd="/tmp", is_first=True,
        skill="none", cli="droid",
    )

    startup_cmd = calls[0]
    assert "--settings <(echo" in startup_cmd
    assert "sessionDefaultSettings" in startup_cmd


def test_spawn_claude_uses_model_flag(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="opus", cwd="/tmp", is_first=True,
        skill="none", cli="claude",
    )

    startup_cmd = calls[0]
    assert "--model 'opus'" in startup_cmd
    assert "claude" in startup_cmd


def test_spawn_codex_uses_model_flag(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="gpt-5.2", cwd="/tmp", is_first=True,
        skill="none", cli="codex",
    )

    startup_cmd = calls[0]
    assert "-m 'gpt-5.2'" in startup_cmd
    assert "codex" in startup_cmd


def test_build_droid_model_settings_resolves_custom_model(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {"model": "opus"},
        "customModels": [
            {"model": "claude-opus-4-6", "displayName": "Claude Opus 4.6", "id": "custom:Claude-Opus-4.6-0"}
        ],
    }))
    monkeypatch.setattr("hive.agent._settings_file", lambda: settings_file)

    json_str, resolved = _build_droid_model_settings("custom:claude-opus-4-6")
    assert resolved == "custom:Claude-Opus-4.6-0"
    assert json_str

    data = json.loads(json_str)
    assert data == {"sessionDefaultSettings": {"model": "custom:Claude-Opus-4.6-0"}}


def test_build_droid_model_settings_keeps_direct_model(monkeypatch, tmp_path):
    settings_file = tmp_path / "settings.json"
    settings_file.write_text(json.dumps({
        "sessionDefaultSettings": {"model": "custom:my-model"},
    }))
    monkeypatch.setattr("hive.agent._settings_file", lambda: settings_file)

    json_str, resolved = _build_droid_model_settings("custom:my-model")
    assert resolved == "custom:my-model"
    data = json.loads(json_str)
    assert data == {"sessionDefaultSettings": {"model": "custom:my-model"}}


def test_build_droid_model_settings_returns_empty_when_no_model():
    json_str, resolved = _build_droid_model_settings("")
    assert json_str == ""
    assert resolved == ""


def test_spawn_rejects_unknown_cli(monkeypatch):
    _setup_tmux_mocks(monkeypatch)

    try:
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none", cli="vim")
    except ValueError as exc:
        assert "unsupported cli" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_spawn_droid_resume_uses_dash_r(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="droid",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert "-r 'sess-abc'" in startup_cmd


def test_spawn_claude_resume_uses_fork_session(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert "-r 'sess-abc'" in startup_cmd
    assert "--fork-session" in startup_cmd


def test_spawn_codex_resume_uses_fork_subcommand(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="codex",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert "codex" in startup_cmd
    assert "fork" in startup_cmd
    assert "sess-abc" in startup_cmd
    # codex fork does not take --model; model flag should not appear
    assert "-m" not in startup_cmd


def test_spawn_claude_skips_droid_session_detection(monkeypatch):
    calls = _setup_tmux_mocks(monkeypatch)
    scanned: list[str] = []
    monkeypatch.setattr("hive.agent._list_sessions", lambda cwd: scanned.append(cwd) or set())

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
    )

    assert scanned == [], "should not scan droid sessions for claude"


def test_detect_new_session_matches_resolved_model_id(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    project_dir = sessions_dir / "-tmp-test"
    project_dir.mkdir(parents=True)
    monkeypatch.setattr("hive.agent._sessions_dir", lambda: sessions_dir)
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda _pane: None)

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
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.agent._list_sessions", lambda _cwd: {"old", "new"})
    monkeypatch.setattr("hive.agent._session_timestamp", lambda _cwd, sid: {"old": 1, "new": 2}[sid])

    assert detect_current_session_id("/tmp/test") == "new"


def test_detect_current_session_prefers_matching_model(monkeypatch):
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.agent._list_sessions", lambda _cwd: {"a", "b"})
    monkeypatch.setattr("hive.agent._session_timestamp", lambda _cwd, sid: {"a": 1, "b": 2}[sid])
    monkeypatch.setattr(
        "hive.agent._read_session_model",
        lambda _cwd, sid: {"a": "custom:Claude-Opus-4.6-0", "b": "custom:GPT-5.4-1"}[sid],
    )

    assert detect_current_session_id("/tmp/test", model="custom:Claude-Opus-4.6-0") == "a"


def test_detect_current_session_prefers_session_map(monkeypatch):
    monkeypatch.setattr(
        "hive.agent.resolve_session_id_for_pane",
        lambda pane_id: "map-sess-1" if pane_id == "%11" else None,
    )

    assert detect_current_session_id("/tmp/test", pane_id="%11") == "map-sess-1"


def test_detect_new_session_prefers_session_map(monkeypatch):
    monkeypatch.setattr(
        "hive.agent.resolve_session_id_for_pane",
        lambda pane_id: "map-sess-2" if pane_id == "%12" else None,
    )

    assert _detect_new_session("/tmp/test", {"older"}, pane_id="%12") == "map-sess-2"
