"""Tests for the crew integration-branch surface.

The integration branch rides its own window option
(@hive-crew-integration-branch) — never @hive-crew-base, which is the crew's
numeric peer-window range base and gets parsed as int / rewritten by
spawn-cell. Covers the setter command, propagation to cell windows, and the
two options' independence.
"""

import json
import subprocess
from pathlib import Path

import pytest

from hive.cli import _copy_crew_integration_option, cli

pytestmark = pytest.mark.cli


def _run(args, cwd):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, f"{args}: {r.stderr}"
    return r.stdout.strip()


@pytest.fixture
def repo(tmp_path: Path, monkeypatch) -> Path:
    main = tmp_path / "repo"
    _run(["git", "init", "-q", str(main)], tmp_path)
    _run(["git", "-C", str(main), "config", "user.email", "t@example.invalid"], tmp_path)
    _run(["git", "-C", str(main), "config", "user.name", "t"], tmp_path)
    _run(["git", "-C", str(main), "commit", "--allow-empty", "-m", "init"], tmp_path)
    monkeypatch.chdir(main)
    return main


def _window_options(target: str = "dev:0") -> dict:
    from hive import cli as cli_mod

    probe = {}
    for key in ("hive-crew-name", "hive-crew-base", "hive-crew-integration-branch"):
        value = cli_mod.tmux.get_window_option(target, key)
        if value is not None:
            probe[key] = value
    return probe


def test_setter_requires_crew_window(runner, configure_hive_home, repo):
    configure_hive_home()
    result = runner.invoke(cli, ["crew", "set-integration-branch", "main"])
    assert result.exit_code == 1
    assert "not in a crew window" in result.output


def test_setter_rejects_unresolvable_ref(runner, configure_hive_home, repo):
    configure_hive_home()
    from hive import cli as cli_mod

    cli_mod.tmux.set_window_option("dev:0", "@hive-crew-name", "epic")
    result = runner.invoke(cli, ["crew", "set-integration-branch", "no-such-branch"])
    assert result.exit_code == 1
    assert "cannot resolve" in result.output
    assert "hive-crew-integration-branch" not in _window_options()


def test_setter_writes_independent_key_and_keeps_numeric_base(runner, configure_hive_home, repo):
    configure_hive_home()
    from hive import cli as cli_mod

    cli_mod.tmux.set_window_option("dev:0", "@hive-crew-name", "epic")
    cli_mod.tmux.set_window_option("dev:0", "@hive-crew-base", "1000")
    _run(["git", "branch", "epic-int"], repo)
    result = runner.invoke(cli, ["crew", "set-integration-branch", "epic-int", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["crew"] == "epic"
    assert payload["integrationBranch"] == "epic-int"
    assert len(payload["oid"]) == 40
    options = _window_options()
    assert options["hive-crew-integration-branch"] == "epic-int"
    assert options["hive-crew-base"] == "1000"  # numeric range base untouched


def test_copy_propagates_only_when_set(configure_hive_home):
    configure_hive_home()
    from hive import cli as cli_mod

    _copy_crew_integration_option("dev:0", "dev:1")
    assert cli_mod.tmux.get_window_option("dev:1", "hive-crew-integration-branch") is None

    cli_mod.tmux.set_window_option("dev:0", "@hive-crew-integration-branch", "epic-int")
    _copy_crew_integration_option("dev:0", "dev:1")
    assert cli_mod.tmux.get_window_option("dev:1", "hive-crew-integration-branch") == "epic-int"


def test_copied_option_feeds_worktree_start_in_cell_window(runner, configure_hive_home, repo, tmp_path):
    """End-to-end at the option layer: crew declares integration, the option is
    copied to the cell window, and `hive worktree start` in that window uses it
    as base + gh-merge-base."""
    configure_hive_home()
    from hive import cli as cli_mod

    remote = tmp_path / "r.git"
    _run(["git", "init", "-q", "--bare", str(remote)], tmp_path)
    default = _run(["git", "symbolic-ref", "--short", "HEAD"], repo)
    _run(["git", "remote", "add", "origin", str(remote)], repo)
    _run(["git", "push", "-q", "-u", "origin", default], repo)
    _run(["git", "remote", "set-head", "origin", "-a"], repo)
    _run(["git", "branch", "epic-int"], repo)

    cli_mod.tmux.set_window_option("crew:9", "@hive-crew-name", "epic")
    cli_mod.tmux.set_window_option("crew:9", "@hive-crew-integration-branch", "epic-int")
    # spawn-cell side: copy into the (faked) current cell window dev:0.
    _copy_crew_integration_option("crew:9", "dev:0")
    cli_mod.tmux.set_window_option("dev:0", "@hive-crew-name", "epic")

    result = runner.invoke(cli, ["worktree", "start", "feat-a", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["base"] == "epic-int"
    assert payload["owner"] == "crew:epic"
    assert _run(["git", "config", "branch.feat-a.gh-merge-base"], repo) == "epic-int"
