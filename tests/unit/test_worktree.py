"""Unit tests for the worktree pool plumbing (hive.worktree).

Real temp git repos, no tmux: this layer covers base resolution, the start
compatibility matrix, done guards, and status classification against actual
git state — git itself is the metadata source of truth, so faking it would
test nothing.
"""

import subprocess
import time
from pathlib import Path

import pytest

from hive import worktree as wt

pytestmark = pytest.mark.unit


def _run(args, cwd):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, f"{args}: {r.stderr}"
    return r.stdout.strip()


def _commit(repo: Path, msg: str) -> str:
    _run(["git", "commit", "--allow-empty", "-m", msg], repo)
    return _run(["git", "rev-parse", "HEAD"], repo)


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """Main checkout with a file remote and origin/HEAD set."""
    remote = tmp_path / "remote.git"
    _run(["git", "init", "-q", "--bare", str(remote)], tmp_path)
    main = tmp_path / "repo"
    _run(["git", "init", "-q", str(main)], tmp_path)
    _run(["git", "-C", str(main), "config", "user.email", "t@example.invalid"], tmp_path)
    _run(["git", "-C", str(main), "config", "user.name", "t"], tmp_path)
    _commit(main, "init")
    default = _run(["git", "symbolic-ref", "--short", "HEAD"], main)
    _run(["git", "remote", "add", "origin", str(remote)], main)
    _run(["git", "push", "-q", "-u", "origin", default], main)
    _run(["git", "remote", "set-head", "origin", "-a"], main)
    return main


def _base(repo: Path) -> wt.BaseResolution:
    return wt.resolve_base(repo, None, None)


def _start(repo: Path, feature: str, *, owner: str = "team:t1", base=None, **kw) -> wt.StartResult:
    return wt.start(repo, feature, base=base or _base(repo), owner=owner, team="t1", **kw)


# --- anchoring -----------------------------------------------------------------

def test_repo_anchor_resolves_main_root_from_linked_worktree(repo):
    res = _start(repo, "feat-a")
    anchor_from_inside = wt.repo_anchor(res.path)
    assert anchor_from_inside == repo.resolve()


def test_start_from_inside_linked_worktree_lands_in_flat_pool(repo):
    res_a = _start(repo, "feat-a")
    anchor = wt.repo_anchor(res_a.path)  # as the CLI would, cwd inside feat-a
    res_b = wt.start(anchor, "feat-b", base=wt.resolve_base(anchor, None, None), owner="team:t1", team="t1")
    assert Path(res_b.path).parent == wt.pool_root(repo.resolve())
    assert not Path(res_b.path).resolve().is_relative_to(Path(res_a.path).resolve())


# --- base resolution -----------------------------------------------------------

def test_resolve_base_detects_default_branch(repo):
    base = wt.resolve_base(repo, None, None)
    assert base.source == "default-branch"
    assert base.ref.startswith("origin/")
    assert len(base.oid) == 40


def test_pr_base_strips_origin_refs_longest_prefix_first():
    assert wt.pr_merge_base_from_ref("origin/main") == "main"
    assert wt.pr_merge_base_from_ref("refs/remotes/origin/main") == "main"
    assert wt.pr_merge_base_from_ref("refs/heads/develop") == "develop"
    assert wt.pr_merge_base_from_ref("main") == "main"


def test_resolve_base_explicit_invalid_ref_fails(repo):
    with pytest.raises(wt.WorktreeError, match="cannot resolve"):
        wt.resolve_base(repo, "no-such-ref", None)


def test_resolve_base_no_origin_head_hard_fails(tmp_path):
    bare = tmp_path / "solo"
    _run(["git", "init", "-q", str(bare)], tmp_path)
    _run(["git", "-C", str(bare), "config", "user.email", "t@example.invalid"], tmp_path)
    _run(["git", "-C", str(bare), "config", "user.name", "t"], tmp_path)
    _commit(bare, "x")
    with pytest.raises(wt.WorktreeError, match="default branch"):
        wt.resolve_base(bare, None, None)


def test_resolve_base_integration_resolves(repo):
    _run(["git", "branch", "epic-integration"], repo)
    base = wt.resolve_base(repo, None, "epic-integration")
    assert base.source == "integration"
    assert base.ref == "epic-integration"


# --- start matrix ----------------------------------------------------------------

def test_start_creates_worktree_branch_and_meta(repo):
    res = _start(repo, "feat-a")
    assert res.mode == "created" and res.ready
    assert Path(res.path) == wt.feature_path(repo.resolve(), "feat-a")
    assert Path(res.path).is_dir()
    meta = wt.read_meta(repo, "feat-a")
    assert meta["hive-owner"] == "team:t1"
    assert meta["hive-base"] == res.base
    assert meta["hive-base-oid"] == res.base_oid
    assert "hive-created" in meta


def test_start_is_idempotent_for_existing_worktree(repo):
    first = _start(repo, "feat-a")
    second = _start(repo, "feat-a")
    assert second.mode == "existing" and second.ready
    assert second.path == first.path


def test_start_foreign_owner_hard_fails_without_config_overwrite(repo):
    _start(repo, "feat-a")
    _run(["git", "config", "branch.feat-a.hive-owner", "team:other"], repo)
    with pytest.raises(wt.WorktreeError, match="owned by 'team:other'"):
        _start(repo, "feat-a")
    assert wt.read_meta(repo, "feat-a")["hive-owner"] == "team:other"


def test_start_needs_rebase_when_base_advances(repo):
    _start(repo, "feat-a")
    new_oid = _commit(repo, "advance")
    stored_before = wt.read_meta(repo, "feat-a")["hive-base-oid"]
    res = _start(repo, "feat-a", base=wt.resolve_base(repo, new_oid, None))
    assert res.mode == "needs-rebase" and not res.ready
    assert res.warnings
    # hive-base-oid must not advance until the branch actually contains it.
    assert wt.read_meta(repo, "feat-a")["hive-base-oid"] == stored_before


def test_start_attaches_branch_left_by_manual_worktree_remove(repo):
    res = _start(repo, "feat-a")
    _run(["git", "worktree", "remove", res.path], repo)
    assert wt.read_meta(repo, "feat-a")["hive-owner"] == "team:t1"  # meta survives
    res2 = _start(repo, "feat-a")
    assert res2.mode == "attached" and res2.ready
    assert Path(res2.path).is_dir()


def test_start_adopts_unlabeled_branch_after_done(repo):
    res = _start(repo, "feat-a")
    wt.done(repo, "feat-a", caller_cwd=str(repo))
    res2 = _start(repo, "feat-a")
    assert res2.mode == "adopted-existing-branch" and res2.ready
    assert wt.read_meta(repo, "feat-a")["hive-owner"] == "team:t1"
    assert res2.path == res.path


def test_start_rejects_branch_checked_out_in_main_checkout(repo):
    _run(["git", "checkout", "-q", "-b", "feat-z"], repo)
    with pytest.raises(wt.WorktreeError, match="already checked out at"):
        _start(repo, "feat-z")


def test_start_recovers_stale_worktree_entry(repo):
    res = _start(repo, "feat-a")
    import shutil

    shutil.rmtree(res.path)  # simulate manual rm -rf, leaving a prunable entry
    res2 = _start(repo, "feat-a")
    assert res2.ready
    assert Path(res2.path).is_dir()


def test_start_integration_writes_gh_merge_base(repo):
    _run(["git", "branch", "epic-int"], repo)
    base = wt.resolve_base(repo, None, "epic-int")
    res = wt.start(repo, "feat-a", base=base, owner="team:epic", gh_merge_base="epic-int")
    assert res.ready
    meta = wt.read_meta(repo, "feat-a")
    assert meta["gh-merge-base"] == "epic-int"


def test_start_standalone_writes_gh_merge_base_from_origin_base(repo):
    base = wt.BaseResolution(
        ref="origin/main",
        oid=_run(["git", "rev-parse", "HEAD"], repo),
        source="default-branch",
    )
    res = wt.start(repo, "feat-a", base=base, owner="team:t1", team="t1")
    assert res.ready
    assert wt.read_meta(repo, "feat-a")["gh-merge-base"] == "main"


def test_start_rejects_invalid_feature_name(repo):
    with pytest.raises(wt.WorktreeError, match="invalid feature name"):
        _start(repo, "feat..bad")


def test_start_existing_refreshes_gh_merge_base_when_integration_moves(repo):
    _run(["git", "branch", "old-int"], repo)
    _run(["git", "branch", "new-int"], repo)
    base_old = wt.resolve_base(repo, None, "old-int")
    wt.start(repo, "feat-a", base=base_old, owner="team:epic", gh_merge_base="old-int")
    created = wt.read_meta(repo, "feat-a")["hive-created"]
    base_new = wt.resolve_base(repo, None, "new-int")
    res = wt.start(repo, "feat-a", base=base_new, owner="team:epic", gh_merge_base="new-int")
    assert res.mode == "existing"
    meta = wt.read_meta(repo, "feat-a")
    assert meta["gh-merge-base"] == "new-int"
    assert meta["hive-base"] == "new-int"
    assert meta["hive-created"] == created  # first-created timestamp survives


def test_start_attach_backfills_stale_gh_merge_base(repo):
    _run(["git", "branch", "old-int"], repo)
    _run(["git", "branch", "new-int"], repo)
    base_old = wt.resolve_base(repo, None, "old-int")
    res = wt.start(repo, "feat-a", base=base_old, owner="team:epic", gh_merge_base="old-int")
    _run(["git", "worktree", "remove", res.path], repo)  # meta survives manual remove
    base_new = wt.resolve_base(repo, None, "new-int")
    res2 = wt.start(repo, "feat-a", base=base_new, owner="team:epic", gh_merge_base="new-int")
    assert res2.mode == "attached"
    assert wt.read_meta(repo, "feat-a")["gh-merge-base"] == "new-int"


# --- done -------------------------------------------------------------------------

def test_done_removes_worktree_keeps_branch_clears_hive_meta_only(repo):
    _run(["git", "branch", "epic-int"], repo)
    base = wt.resolve_base(repo, None, "epic-int")
    res = wt.start(repo, "feat-a", base=base, owner="team:epic", gh_merge_base="epic-int")
    out = wt.done(repo, "feat-a", caller_cwd=str(repo))
    assert out.branch_kept and not Path(res.path).exists()
    assert "hive-owner" in out.cleared_config_keys
    meta = wt.read_meta(repo, "feat-a")
    assert not any(k.startswith("hive-") for k in meta)
    # gh-merge-base survives done: the branch and its PR are still alive.
    assert meta.get("gh-merge-base") == "epic-int"
    assert _run(["git", "branch", "--list", "feat-a"], repo)


def test_done_refuses_from_inside_the_worktree(repo):
    res = _start(repo, "feat-a")
    with pytest.raises(wt.WorktreeError, match="leave the worktree first"):
        wt.done(repo, "feat-a", caller_cwd=res.path)
    assert Path(res.path).exists()


def test_done_refuses_dirty_without_force(repo):
    res = _start(repo, "feat-a")
    (Path(res.path) / "junk.txt").write_text("x")
    with pytest.raises(wt.WorktreeError, match="uncommitted changes"):
        wt.done(repo, "feat-a", caller_cwd=str(repo))


def test_done_refuses_in_progress_operation(repo):
    res = _start(repo, "feat-a")
    merge_head = _run(["git", "rev-parse", "--git-path", "MERGE_HEAD"], Path(res.path))
    target = Path(merge_head) if Path(merge_head).is_absolute() else Path(res.path) / merge_head
    target.write_text(_run(["git", "rev-parse", "HEAD"], repo) + "\n")
    with pytest.raises(wt.WorktreeError, match="merge in progress"):
        wt.done(repo, "feat-a", caller_cwd=str(repo))


def test_done_force_emits_summary_with_untracked_and_ignored_note(repo):
    res = _start(repo, "feat-a")
    (Path(res.path) / "junk.txt").write_text("x")
    out = wt.done(repo, "feat-a", force=True, caller_cwd=str(repo))
    assert out.forced
    assert "junk.txt" in out.status_summary
    assert "ignored files are not included" in out.status_summary
    assert not Path(res.path).exists()


def test_done_force_on_clean_worktree_still_emits_summary(repo):
    res = _start(repo, "feat-a")
    out = wt.done(repo, "feat-a", force=True, caller_cwd=str(repo))
    assert out.forced
    assert out.status_summary  # auditability: --force always reports
    assert "ignored files are not included" in out.status_summary
    assert not Path(res.path).exists()


def test_done_without_worktree_fails_with_status_hint(repo):
    _run(["git", "branch", "feat-only"], repo)
    with pytest.raises(wt.WorktreeError, match="no worktree found"):
        wt.done(repo, "feat-only", caller_cwd=str(repo))


# --- status ------------------------------------------------------------------------

def test_status_active(repo):
    _start(repo, "feat-a")
    s = wt.feature_status(repo, "feat-a")
    assert s["state"] == "active" and s["branchExists"] and not s["dirty"]
    assert s["owner"] == "team:t1"
    assert s["baseOid"] == s["currentBaseOid"]


def test_status_branch_only_after_done(repo):
    _start(repo, "feat-a")
    wt.done(repo, "feat-a", caller_cwd=str(repo))
    s = wt.feature_status(repo, "feat-a")
    assert s["state"] == "branch-only" and s["worktreePath"] == ""


def test_status_needs_rebase_after_base_advances(repo):
    _start(repo, "feat-a")
    _commit(repo, "advance")
    _run(["git", "push", "-q", "origin", "HEAD"], repo)
    _run(["git", "fetch", "-q", "origin"], repo)
    s = wt.feature_status(repo, "feat-a")
    assert s["state"] == "needs-rebase"
    assert s["baseOid"] != s["currentBaseOid"]


def test_status_unknown_branch(repo):
    s = wt.feature_status(repo, "ghost")
    assert s["state"] == "unknown-branch" and not s["branchExists"]


def test_status_dirty_and_in_progress_flags(repo):
    res = _start(repo, "feat-a")
    (Path(res.path) / "junk.txt").write_text("x")
    s = wt.feature_status(repo, "feat-a")
    assert s["dirty"] is True


def test_pool_status_lists_labeled_and_checked_out(repo):
    _start(repo, "feat-a")
    _start(repo, "feat-b")
    wt.done(repo, "feat-b", caller_cwd=str(repo))  # unlabeled now, but...
    _run(["git", "config", "branch.feat-b.hive-owner", "team:t1"], repo)  # relabel
    rows = wt.pool_status(repo)
    features = {r["feature"] for r in rows}
    assert {"feat-a", "feat-b"} <= features
