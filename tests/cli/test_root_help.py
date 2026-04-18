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


def test_root_cli_fails_when_current_agent_pane_skill_is_stale(runner, monkeypatch):
    monkeypatch.setattr(
        "hive.cli.skill_sync.diagnose_hive_skill",
        lambda _cli: {"state": "stale", "cli": "codex", "installedPath": "/x/SKILL.md", "expectedHash": "a", "actualHash": "b"},
    )
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%1")
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _pane, _key: "")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: SimpleNamespace(name="codex"))

    result = runner.invoke(cli, ["current"])

    assert result.exit_code == 1
    assert "stale" in result.output


def test_root_cli_allows_doctor_even_when_skill_is_stale(runner, monkeypatch):
    """Doctor must still run so the user can diagnose the drift."""
    diagnosed: list[str] = []

    def fake_diagnose(cli_name):
        diagnosed.append(cli_name)
        return {"state": "stale", "cli": cli_name, "installedPath": "/x", "expectedHash": "a", "actualHash": "b"}

    monkeypatch.setattr("hive.cli.skill_sync.diagnose_hive_skill", fake_diagnose)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%1")
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _pane, _key: "")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: SimpleNamespace(name="codex"))

    # The bypass short-circuits before diagnose even runs.
    result = runner.invoke(cli, ["doctor", "--help"])

    assert result.exit_code == 0
    assert diagnosed == []


def test_root_cli_skips_skill_check_for_non_agent_pane(runner, monkeypatch):
    diagnosed: list[str] = []

    def fake_diagnose(cli_name):
        diagnosed.append(cli_name)
        return {"state": "stale"}

    monkeypatch.setattr("hive.cli.skill_sync.diagnose_hive_skill", fake_diagnose)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: True)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%1")
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _pane, _key: "")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: None)
    monkeypatch.setattr("hive.cli.plugin_manager.list_plugins", lambda: [])

    result = runner.invoke(cli, ["plugin", "list"])

    assert result.exit_code == 0
    assert diagnosed == []
