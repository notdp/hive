from __future__ import annotations

from types import SimpleNamespace

from hive.cli import cli


def test_root_help_layers_daily_panes_debug_sections(runner):
    result = runner.invoke(cli, ["--help"])

    assert result.exit_code == 0
    for section in ("Daily", "Panes", "Debug"):
        assert section in result.output

    daily_start = result.output.index("Daily")
    panes_start = result.output.index("Panes")
    debug_start = result.output.index("Debug")
    assert daily_start < panes_start < debug_start

    daily_block = result.output[daily_start:panes_start]
    panes_block = result.output[panes_start:debug_start]
    debug_block = result.output[debug_start:]

    for command_name in ("team", "send", "notify"):
        assert command_name in daily_block
    for command_name in ("fork", "spawn"):
        assert command_name in panes_block
    for command_name in ("doctor", "delivery", "thread"):
        assert command_name in debug_block
    assert "reply" not in daily_block
    assert 'hive send dodo "see report" --artifact - <<\'EOF\'' in result.output
