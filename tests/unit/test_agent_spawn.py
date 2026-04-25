"""Tests for Agent.spawn model/skill/env handling."""

from hive.agent import (
    Agent,
    _build_droid_model_settings,
    detect_current_session_id,
)
import json


def _setup_tmux_mocks(monkeypatch):
    calls: list[str] = []
    tags: list[tuple[object, ...]] = []

    monkeypatch.setattr("hive.agent.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.agent.tmux.split_window", lambda target, horizontal=True, size=None, cwd=None: target)
    monkeypatch.setattr("hive.agent.tmux.get_pane_tty", lambda _pane: None)
    monkeypatch.setattr("hive.agent.tmux.set_pane_title", lambda *_: None)
    monkeypatch.setattr("hive.agent.tmux.tag_pane", lambda *args, **_kwargs: tags.append(args))
    monkeypatch.setattr("hive.agent.tmux.wait_for_text", lambda *_args, **_kw: True)
    monkeypatch.setattr("hive.agent.tmux.wait_for_texts", lambda *_args, **_kw: True)
    monkeypatch.setattr("hive.agent.tmux.is_pane_in_mode", lambda _pane: False)
    monkeypatch.setattr("hive.agent.tmux.cancel_pane_mode", lambda _pane: None)
    def _send_keys(_pane, text, enter=True):
        calls.append(text)
        if enter:
            calls.append("<Enter>")
    monkeypatch.setattr("hive.agent.tmux.send_keys", _send_keys)
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda _pane, key: calls.append(f"<{key}>"))
    monkeypatch.setattr("hive.agent.draft_guard.supported_profile", lambda _profile: False)
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.agent.time.sleep", lambda *_: None)
    monkeypatch.setattr("hive.agent.skill_sync.maybe_warn_hive_skill_drift", lambda *_args, **_kwargs: None)

    return calls, tags


def test_spawn_rejects_outside_tmux(monkeypatch):
    monkeypatch.setattr("hive.agent.tmux.is_inside_tmux", lambda: False)

    try:
        Agent.spawn(name="w1", team_name="t", target_pane="%0", cwd="/tmp", skill="none")
    except ValueError as exc:
        assert "requires tmux" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_spawn_loads_specified_skill(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="", cwd="/tmp", is_first=True,
        skill="code-review",
    )

    assert "/code-review" in calls
    # Should NOT send hive bootstrap message
    assert not any("hive teammate" in c for c in calls)


def test_spawn_skips_skill_when_none(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    assert not any(c.startswith("/") and not c.startswith("/tmp") for c in calls)


def test_spawn_passes_extra_env(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

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
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none",
    )

    startup_cmd = calls[0]
    assert "HIVE_TEAM_NAME=" not in startup_cmd
    assert "HIVE_AGENT_NAME=" not in startup_cmd
    assert "export " not in startup_cmd


def test_spawn_hive_loads_skill_and_sends_prompt(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="hive",
        prompt="Please check your inbox.",
    )

    assert "/hive" in calls
    assert "Please check your inbox." in calls


def test_spawn_codex_hive_loads_skill_and_sends_prompt(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="hive",
        prompt="Please check your inbox.", cli="codex",
    )

    assert "$hive" in calls
    assert "Please check your inbox." in calls
    # 4 Enters: initial `cd ... && exec codex`; skill load is 2 Enters for
    # codex (picker select + submit); then user prompt.
    assert calls.count("<Enter>") == 4


def test_load_skill_sends_slash_command(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    agent = Agent(name="w1", team_name="t", pane_id="%0")

    agent.load_skill("code-review")

    assert calls == ["/code-review", "<Enter>"]


def test_load_skill_uses_cli_specific_command(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    agent = Agent(name="w1", team_name="t", pane_id="%0", cli="codex")

    agent.load_skill("code-review")

    # codex skill picker needs two Enters: first picks the entry, second submits.
    assert calls == ["$code-review", "<Enter>", "<Enter>"]


def test_load_hive_skill_checks_for_drift_before_loading(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    warned: list[str] = []
    monkeypatch.setattr("hive.agent.skill_sync.maybe_warn_hive_skill_drift", lambda cli: warned.append(cli))
    agent = Agent(name="w1", team_name="t", pane_id="%0", cli="codex")

    agent.load_skill("hive")

    assert warned == ["codex"]
    assert calls == ["$hive", "<Enter>", "<Enter>"]


def test_send_submits_text_with_enter(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    agent = Agent(name="w1", team_name="t", pane_id="%0", cli="codex")

    agent.send("hello world")

    assert calls == ["hello world", "<Enter>"]


def test_send_same_for_droid(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    agent = Agent(name="w1", team_name="t", pane_id="%0", cli="droid")

    agent.send("hello world")

    assert calls == ["hello world", "<Enter>"]


def test_send_exits_tmux_copy_mode_before_submitting(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)
    canceled: list[str] = []
    monkeypatch.setattr("hive.agent.tmux.is_pane_in_mode", lambda _pane: True)
    monkeypatch.setattr("hive.agent.tmux.cancel_pane_mode", lambda pane: canceled.append(pane))
    agent = Agent(name="w1", team_name="t", pane_id="%0", cli="claude")

    agent.send("hello world")

    assert canceled == ["%0"]
    assert calls == ["hello world", "<Enter>"]


def test_spawn_droid_uses_temp_settings_file_for_model(monkeypatch):
    calls, tags = _setup_tmux_mocks(monkeypatch)

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
    assert 'settings_file=$(mktemp "${TMPDIR:-/tmp}/hive-droid-settings.XXXXXX")' in startup_cmd
    assert "--settings \"$settings_file\"" in startup_cmd
    assert "sessionDefaultSettings" in startup_cmd
    assert tags == [("%0", "agent", "w1", "t")]
    _assert_startup_cmd_runs_on_bash(startup_cmd)


def _assert_startup_cmd_runs_on_bash(startup_cmd: str) -> None:
    """Run the generated startup_cmd on the local bash up to (but excluding)
    the final `exec <cli>` step, to catch shell-level failures like GNU
    `mktemp -t TEMPLATE` rejecting templates without 3+ X's.

    The real command ends with `exec '.../droid' ...`; we stop before exec
    by replacing the droid invocation with `true`, then ensure the settings
    file was actually created.
    """
    import os
    import shlex
    import shutil
    import subprocess

    if shutil.which("bash") is None:
        return

    # Trim everything from the final `exec '` onwards and replace with a no-op
    # that still consumes the trailing `--settings "$settings_file"` args.
    exec_idx = startup_cmd.rfind(" && exec '")
    assert exec_idx > 0, f"startup_cmd missing exec step: {startup_cmd!r}"
    harness = startup_cmd[:exec_idx] + ' && test -s "$settings_file"'

    result = subprocess.run(
        ["bash", "-c", harness],
        capture_output=True,
        text=True,
        timeout=5,
        env={**os.environ, "TMPDIR": "/tmp"},
    )
    assert result.returncode == 0, (
        "generated startup_cmd failed to execute on bash\n"
        f"cmd={harness!r}\nstdout={result.stdout!r}\nstderr={result.stderr!r}"
    )


def test_spawn_tags_pane_before_waiting_for_ready(monkeypatch):
    calls, tags = _setup_tmux_mocks(monkeypatch)
    monkeypatch.setattr("hive.agent.tmux.wait_for_texts", lambda *_args, **_kw: False)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%9",
        cwd="/tmp", is_first=True, skill="none", cli="droid",
    )

    assert calls, "spawn should still start the CLI process"
    assert tags == [("%9", "agent", "w1", "t")]


def test_spawn_claude_uses_model_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        model="opus", cwd="/tmp", is_first=True,
        skill="none", cli="claude",
    )

    startup_cmd = calls[0]
    assert "--model 'opus'" in startup_cmd
    assert "claude" in startup_cmd


def test_spawn_codex_uses_model_flag(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

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
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="droid",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert "-r 'sess-abc'" in startup_cmd


def test_spawn_claude_resume_uses_fork_session(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
        session_id="sess-abc",
    )

    startup_cmd = calls[0]
    assert "-r 'sess-abc'" in startup_cmd
    assert "--fork-session" in startup_cmd


def test_spawn_codex_resume_uses_fork_subcommand(monkeypatch):
    calls, _ = _setup_tmux_mocks(monkeypatch)

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
    calls, _ = _setup_tmux_mocks(monkeypatch)
    resolved: list[str] = []
    monkeypatch.setattr("hive.agent.resolve_session_id_for_pane", lambda pane_id: resolved.append(pane_id) or None)

    Agent.spawn(
        name="w1", team_name="t", target_pane="%0",
        cwd="/tmp", is_first=True, skill="none", cli="claude",
    )

    assert resolved == [], "should not resolve session for claude"


def test_detect_current_session_id_delegates_to_resolve(monkeypatch):
    monkeypatch.setattr(
        "hive.agent.resolve_session_id_for_pane",
        lambda pane_id: "map-sess-1" if pane_id == "%11" else None,
    )

    assert detect_current_session_id("/tmp/test", pane_id="%11") == "map-sess-1"
    assert detect_current_session_id("/tmp/test", pane_id="%99") is None
