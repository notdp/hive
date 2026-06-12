"""Tests for `hive config roles` — show configured role overrides."""

import json

import pytest

from hive.cli import cli
from hive.settings import APPLIED_ROLES, CONFIGURABLE_ROLES


def test_config_roles_empty(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)
    result = runner.invoke(cli, ["config", "roles"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert set(data.keys()) == CONFIGURABLE_ROLES
    for role, info in data.items():
        assert info["cli"] == ""
        assert info["model"] == ""
        assert info["applied"] == (role in APPLIED_ROLES)


def test_config_roles_with_overrides(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
            "roles.validator.model": "o3",
            "roles.worker.model": "opus",
        }.get(key, default),
    )
    result = runner.invoke(cli, ["config", "roles"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert data["validator"]["cli"] == "codex"
    assert data["validator"]["model"] == "o3"
    assert data["worker"]["model"] == "opus"
    assert data["worker"]["cli"] == ""
    assert data["orch"]["applied"] is False
    assert data["challenger"]["applied"] is True
