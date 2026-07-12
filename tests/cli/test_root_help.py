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

    for command_name in ("team", "send", "reply", "notify"):
        assert command_name in daily_block
    for command_name in ("handoff", "fork", "spawn"):
        assert command_name in handoff_block
    for command_name in ("doctor", "delivery", "thread"):
        assert command_name in debug_block
    assert 'hive send dodo "see report" --artifact - <<\'EOF\'' in result.output


