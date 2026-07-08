import json

import pytest

from hive.cli import cli


pytestmark = pytest.mark.cli


def test_config_set_get_round_trip(runner, configure_hive_home):
    configure_hive_home()

    set_result = runner.invoke(cli, ["config", "set", "notify.enabled", "true"])
    assert set_result.exit_code == 0
    assert json.loads(set_result.output.strip()) is True

    get_result = runner.invoke(cli, ["config", "get", "notify.enabled"])
    assert get_result.exit_code == 0
    assert json.loads(get_result.output.strip()) is True


def test_config_get_missing_key_exits_nonzero(runner, configure_hive_home):
    configure_hive_home()
    result = runner.invoke(cli, ["config", "get", "missing.key"])
    assert result.exit_code == 1


def test_config_set_parses_int_and_string(runner, configure_hive_home):
    configure_hive_home()

    runner.invoke(cli, ["config", "set", "tunables.delay", "42"])
    runner.invoke(cli, ["config", "set", "tunables.label", "fast"])

    delay = runner.invoke(cli, ["config", "get", "tunables.delay"])
    label = runner.invoke(cli, ["config", "get", "tunables.label"])
    assert json.loads(delay.output.strip()) == 42
    assert json.loads(label.output.strip()) == "fast"


def test_config_unset_removes_key(runner, configure_hive_home):
    configure_hive_home()
    runner.invoke(cli, ["config", "set", "notify.enabled", "true"])

    unset_result = runner.invoke(cli, ["config", "unset", "notify.enabled"])
    assert unset_result.exit_code == 0

    get_result = runner.invoke(cli, ["config", "get", "notify.enabled"])
    assert get_result.exit_code == 1


def test_config_unset_missing_key_exits_nonzero(runner, configure_hive_home):
    configure_hive_home()
    result = runner.invoke(cli, ["config", "unset", "never.set"])
    assert result.exit_code == 1


def test_config_works_outside_tmux(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)
    set_result = runner.invoke(cli, ["config", "set", "notify.enabled", "true"])
    assert set_result.exit_code == 0
    get_result = runner.invoke(cli, ["config", "get", "notify.enabled"])
    assert get_result.exit_code == 0
    assert json.loads(get_result.output.strip()) is True
