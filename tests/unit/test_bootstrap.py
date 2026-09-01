"""Unit tests for the plugin bootstrap hook's CLI check phase."""
import importlib.util
import subprocess
import sys
from pathlib import Path

import pytest

_SPEC = importlib.util.spec_from_file_location(
    "hive_plugin_bootstrap",
    Path(__file__).resolve().parents[2] / "plugins/hive/scripts/bootstrap.py",
)
bootstrap = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(bootstrap)


def _runner_for(stdout: str, returncode: int = 0):
    def runner(argv, capture_output, text, timeout):
        return subprocess.CompletedProcess(argv, returncode, stdout=stdout, stderr="")
    return runner


def test_ensure_cli_missing_binary_points_at_installer():
    with pytest.raises(bootstrap.BootstrapError) as e:
        bootstrap.ensure_cli(which=lambda name: None)
    assert "not on PATH" in str(e.value)
    assert "hive-installer.sh" in str(e.value)


def test_ensure_cli_old_binary_points_at_installer():
    runner = _runner_for("hive, version 0.10.1\n")
    with pytest.raises(bootstrap.BootstrapError) as e:
        bootstrap.ensure_cli(which=lambda name: "/usr/local/bin/hive", runner=runner)
    assert "0.10.1" in str(e.value)
    assert "hive-installer.sh" in str(e.value)


def test_ensure_cli_current_binary_converges():
    version = ".".join(map(str, bootstrap.MIN_CLI_VERSION))
    runner = _runner_for(f"hive, version {version}\n")
    summary = bootstrap.ensure_cli(which=lambda name: "/usr/local/bin/hive", runner=runner)
    assert "meets minimum" in summary


def test_ensure_cli_unparseable_version_fails_loudly():
    runner = _runner_for("hive 999 weird build\n")
    with pytest.raises(bootstrap.BootstrapError) as e:
        bootstrap.ensure_cli(which=lambda name: "/usr/local/bin/hive", runner=runner)
    assert "cannot parse" in str(e.value)
