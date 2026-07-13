"""Unit tests for `_feature_title_for_cwd` — set-pr's default window title.

Real temp git repos: the helper's whole job is classifying actual git state
(hive-started worktree vs. bare branch vs. detached vs. no repo), so faking
git would test nothing. Only branches with real ``hive-*`` metadata qualify;
``gh-merge-base`` alone (what ``worktree done`` leaves behind) does not.
"""

import subprocess
from pathlib import Path

import pytest

from hive import worktree as wt
from hive.cli import _feature_title_for_cwd

pytestmark = pytest.mark.unit


def _run(args, cwd):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, f"{args}: {r.stderr}"
    return r.stdout.strip()


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """Main checkout with a file remote and origin/HEAD set."""
    remote = tmp_path / "remote.git"
    _run(["git", "init", "-q", "--bare", str(remote)], tmp_path)
    main = tmp_path / "repo"
    _run(["git", "init", "-q", str(main)], tmp_path)
    _run(["git", "-C", str(main), "config", "user.email", "t@example.invalid"], tmp_path)
    _run(["git", "-C", str(main), "config", "user.name", "t"], tmp_path)
    _run(["git", "commit", "--allow-empty", "-m", "init"], main)
    default = _run(["git", "symbolic-ref", "--short", "HEAD"], main)
    _run(["git", "remote", "add", "origin", str(remote)], main)
    _run(["git", "push", "-q", "-u", "origin", default], main)
    _run(["git", "remote", "set-head", "origin", "-a"], main)
    return main


def test_hive_started_worktree_yields_feature_name(repo):
    res = wt.start(repo, "feat-a", base=wt.resolve_base(repo, None, None), owner="team:t1", team="t1")
    assert _feature_title_for_cwd(res.path) == "feat-a"


def test_branch_with_only_gh_merge_base_yields_nothing(repo):
    _run(["git", "checkout", "-q", "-b", "feat-b"], repo)
    _run(["git", "config", "branch.feat-b.gh-merge-base", "main"], repo)
    assert _feature_title_for_cwd(str(repo)) == ""


def test_plain_branch_yields_nothing(repo):
    _run(["git", "checkout", "-q", "-b", "feat-c"], repo)
    assert _feature_title_for_cwd(str(repo)) == ""


def test_detached_head_yields_nothing(repo):
    _run(["git", "checkout", "-q", "--detach"], repo)
    assert _feature_title_for_cwd(str(repo)) == ""


def test_non_repo_cwd_yields_nothing(tmp_path):
    assert _feature_title_for_cwd(str(tmp_path)) == ""
