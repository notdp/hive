"""CLI tests for `hive worktree start/done/status`.

Real temp git repos for the git side; tmux context (crew window options,
team binding) comes from the conftest fake-tmux fixture. Covers the --json
schemas, the base hard-fail matrix at the command layer, crew propagation
into gh-merge-base, and help placement.
"""

import json
import subprocess
from pathlib import Path

import pytest

from hive.cli import cli

pytestmark = pytest.mark.cli


def _run(args, cwd):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, f"{args}: {r.stderr}"
    return r.stdout.strip()


@pytest.fixture
def repo(tmp_path: Path, monkeypatch) -> Path:
    remote = tmp_path / "remote.git"
    _run(["git", "init", "-q", "--bare", str(remote)], tmp_path)
    main = tmp_path / "repo"
    _run(["git", "init", "-q", str(main)], tmp_path)
    _run(["git", "-C", str(main), "config", "user.email", "t@example.invalid"], tmp_path)
    _run(["git", "-C", str(main), "config", "user.name", "t"], tmp_path)
    _run(["git", "-C", str(main), "commit", "--allow-empty", "-m", "init"], tmp_path)
    default = _run(["git", "symbolic-ref", "--short", "HEAD"], main)
    _run(["git", "remote", "add", "origin", str(remote)], main)
    _run(["git", "push", "-q", "-u", "origin", default], main)
    _run(["git", "remote", "set-head", "origin", "-a"], main)
    monkeypatch.chdir(main)
    return main


def _set_crew_window(target: str = "dev:0", crew: str = "epic", integration: str | None = "epic-int"):
    from hive import cli as cli_mod

    cli_mod.tmux.set_window_option(target, "@hive-crew-name", crew)
    if integration is not None:
        cli_mod.tmux.set_window_option(target, "@hive-crew-integration-branch", integration)


def test_start_json_schema_and_created(runner, configure_hive_home, repo):
    configure_hive_home()
    result = runner.invoke(cli, ["worktree", "start", "feat-a", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert set(payload) == {
        "feature", "branch", "path", "mode", "owner", "team", "crewName",
        "base", "baseOid", "worktreeRoot", "gitCommonDir", "warnings",
    }
    assert payload["mode"] == "created"
    assert payload["warnings"] == []
    assert payload["path"].endswith(".claude/worktrees/feat-a")
    assert Path(payload["path"]).is_dir()


def test_start_unbound_owner_outside_team(runner, configure_hive_home, repo):
    configure_hive_home()
    result = runner.invoke(cli, ["worktree", "start", "feat-a", "--json"])
    payload = json.loads(result.output)
    assert payload["owner"] == "unbound"


def test_start_needs_rebase_exits_nonzero_with_json(runner, configure_hive_home, repo):
    configure_hive_home()
    assert runner.invoke(cli, ["worktree", "start", "feat-a", "--json"]).exit_code == 0
    _run(["git", "commit", "--allow-empty", "-m", "advance"], repo)
    new_oid = _run(["git", "rev-parse", "HEAD"], repo)
    result = runner.invoke(cli, ["worktree", "start", "feat-a", "--base", new_oid, "--json"])
    assert result.exit_code == 1
    payload = json.loads(result.output)
    assert payload["mode"] == "needs-rebase"
    assert payload["warnings"]


def test_start_crew_context_writes_gh_merge_base(runner, configure_hive_home, repo):
    configure_hive_home()
    _run(["git", "branch", "epic-int"], repo)
    _set_crew_window()
    result = runner.invoke(cli, ["worktree", "start", "feat-a", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["owner"] == "crew:epic"
    assert payload["crewName"] == "epic"
    assert payload["base"] == "epic-int"
    assert _run(["git", "config", "branch.feat-a.gh-merge-base"], repo) == "epic-int"


def test_start_crew_missing_integration_hard_fails(runner, configure_hive_home, repo):
    configure_hive_home()
    _set_crew_window(integration=None)
    result = runner.invoke(cli, ["worktree", "start", "feat-a"])
    assert result.exit_code == 1
    assert "integration branch" in result.output
    assert "--base" in result.output


def test_start_explicit_base_overrides_crew_integration(runner, configure_hive_home, repo):
    configure_hive_home()
    _run(["git", "branch", "epic-int"], repo)
    _run(["git", "branch", "other-base"], repo)
    _set_crew_window()
    result = runner.invoke(cli, ["worktree", "start", "feat-a", "--base", "other-base", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["base"] == "other-base"
    # gh-merge-base still aims sub-PRs at the integration branch (D2).
    assert _run(["git", "config", "branch.feat-a.gh-merge-base"], repo) == "epic-int"


def test_start_outside_git_repo_fails(runner, configure_hive_home, tmp_path, monkeypatch):
    configure_hive_home()
    outside = tmp_path / "plain"
    outside.mkdir()
    monkeypatch.chdir(outside)
    result = runner.invoke(cli, ["worktree", "start", "feat-a"])
    assert result.exit_code == 1
    assert "not inside a git repository" in result.output


def test_done_json_fields_and_branch_kept(runner, configure_hive_home, repo):
    configure_hive_home()
    runner.invoke(cli, ["worktree", "start", "feat-a", "--json"])
    result = runner.invoke(cli, ["worktree", "done", "feat-a", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert set(payload) == {
        "feature", "branch", "removedPath", "branchKept",
        "clearedConfigKeys", "forced", "statusSummary", "warnings",
    }
    assert payload["branchKept"] is True
    assert "hive-owner" in payload["clearedConfigKeys"]
    assert _run(["git", "branch", "--list", "feat-a"], repo)


def test_done_force_reports_summary(runner, configure_hive_home, repo):
    configure_hive_home()
    start = json.loads(runner.invoke(cli, ["worktree", "start", "feat-a", "--json"]).output)
    (Path(start["path"]) / "junk.txt").write_text("x")
    plain = runner.invoke(cli, ["worktree", "done", "feat-a"])
    assert plain.exit_code == 1
    assert "uncommitted changes" in plain.output
    forced = runner.invoke(cli, ["worktree", "done", "feat-a", "--force", "--json"])
    assert forced.exit_code == 0, forced.output
    payload = json.loads(forced.output)
    assert payload["forced"] is True
    assert "junk.txt" in payload["statusSummary"]


def test_status_single_and_pool_json(runner, configure_hive_home, repo):
    configure_hive_home()
    runner.invoke(cli, ["worktree", "start", "feat-a", "--json"])
    single = runner.invoke(cli, ["worktree", "status", "feat-a", "--json"])
    assert single.exit_code == 0, single.output
    payload = json.loads(single.output)
    assert set(payload) == {
        "feature", "branchExists", "worktreePath", "owner", "base", "baseOid",
        "currentBaseOid", "state", "dirty", "inProgress", "stale", "warnings",
    }
    assert payload["state"] == "active"
    pool = runner.invoke(cli, ["worktree", "status", "--json"])
    rows = json.loads(pool.output)
    assert [r for r in rows if r["feature"] == "feat-a"]


def test_status_is_read_only(runner, configure_hive_home, repo):
    configure_hive_home()
    runner.invoke(cli, ["worktree", "start", "feat-a", "--json"])
    before = _run(["git", "config", "--get-regexp", r"branch\.feat-a\."], repo)
    runner.invoke(cli, ["worktree", "status", "feat-a", "--json"])
    runner.invoke(cli, ["worktree", "status", "--json"])
    after = _run(["git", "config", "--get-regexp", r"branch\.feat-a\."], repo)
    assert before == after


def test_worktree_listed_under_workflow_help(runner):
    result = runner.invoke(cli, ["--help"])
    assert result.exit_code == 0
    workflow_idx = result.output.index("Workflow")
    team_idx = result.output.index("Team", workflow_idx)
    assert "worktree" in result.output[workflow_idx:team_idx]


def test_worktree_group_help_shows_subcommands(runner):
    result = runner.invoke(cli, ["worktree", "--help"])
    assert result.exit_code == 0
    for sub in ("start", "done", "status"):
        assert sub in result.output
