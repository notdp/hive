from __future__ import annotations

from types import SimpleNamespace

from hive.cli import cli


def test_root_help_layers_daily_handoff_debug_sections(runner):
    result = runner.invoke(cli, ["--help"])

    assert result.exit_code == 0
    for section in ("Daily", "Handoff", "Debug"):
        assert section in result.output

    daily_start = result.output.index("Daily")
    handoff_start = result.output.index("Handoff")
    debug_start = result.output.index("Debug")
    assert daily_start < handoff_start < debug_start

    daily_block = result.output[daily_start:handoff_start]
    handoff_block = result.output[handoff_start:debug_start]
    debug_block = result.output[debug_start:]

    for command_name in ("current", "team", "send", "answer", "notify"):
        assert command_name in daily_block
    for command_name in ("handoff", "fork", "spawn", "workflow"):
        assert command_name in handoff_block
    for command_name in ("doctor", "delivery", "thread"):
        assert command_name in debug_block


def test_root_cli_checks_version_upgrade_before_tmux_guard(runner, monkeypatch):
    called: list[str] = []

    monkeypatch.setattr("hive.cli.skill_sync.check_version_upgrade", lambda: called.append("checked"))
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["team"])

    assert result.exit_code != 0
    assert called == ["checked"]
    assert "requires tmux" in result.output


def test_root_cli_warns_when_current_agent_pane_skill_is_stale(runner, monkeypatch):
    checked: list[str] = []
    warned: list[str] = []

    monkeypatch.setattr("hive.cli.skill_sync.check_version_upgrade", lambda: checked.append("checked"))
    monkeypatch.setattr("hive.cli.skill_sync.maybe_warn_hive_skill_drift", lambda cli_name: warned.append(cli_name))
    monkeypatch.setattr("hive.cli._stderr_is_interactive", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%1")
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _pane, _key: "")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: SimpleNamespace(name="codex"))
    monkeypatch.setattr("hive.cli.plugin_manager.list_plugins", lambda: [])

    result = runner.invoke(cli, ["plugin", "list"])

    assert result.exit_code == 0
    assert checked == ["checked"]
    assert warned == ["codex"]


def test_root_cli_skips_skill_warning_for_non_agent_pane(runner, monkeypatch):
    warned: list[str] = []

    monkeypatch.setattr("hive.cli.skill_sync.check_version_upgrade", lambda: None)
    monkeypatch.setattr("hive.cli.skill_sync.maybe_warn_hive_skill_drift", lambda cli_name: warned.append(cli_name))
    monkeypatch.setattr("hive.cli._stderr_is_interactive", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%1")
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _pane, _key: "")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.cli.plugin_manager.list_plugins", lambda: [])

    result = runner.invoke(cli, ["plugin", "list"])

    assert result.exit_code == 0
    assert warned == []
