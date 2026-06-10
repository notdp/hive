"""Worktree pool plumbing behind `hive worktree start/done/status`.

Pool layout: ``<main checkout root>/.claude/worktrees/<feature>`` — one
worktree per feature branch (branch == feature), per repo. Git itself is the
source of truth: the worktree<->branch<->path mapping is read live from
``git worktree list`` and ownership/audit marks live in
``git config branch.<feature>.hive-*``. Hive keeps no registry file.

Hive only creates/removes worktrees and reads state. Entering/leaving a
worktree is the agent's own move (a CLI subprocess cannot change the agent
process cwd), and gh/PR work stays on the agent side.
"""

from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

POOL_SEGMENTS = (".claude", "worktrees")

# branch.<feature>.<key> written on a ready start; cleared by done.
META_KEYS = ("hive-owner", "hive-team", "hive-squad", "hive-base", "hive-base-oid", "hive-created")
GH_MERGE_BASE_KEY = "gh-merge-base"

_IN_PROGRESS_MARKERS = (
    ("rebase-merge", "rebase"),
    ("rebase-apply", "rebase"),
    ("MERGE_HEAD", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
    ("BISECT_LOG", "bisect"),
)

READY_MODES = ("created", "existing", "attached", "adopted-existing-branch")


class WorktreeError(Exception):
    """User-actionable failure; the message is the CLI error text."""


def _git(args: list[str], cwd: str | None = None, timeout: float = 30.0) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(
            ["git", *args],
            cwd=cwd or None,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except FileNotFoundError:
        raise WorktreeError("git not found on PATH")
    except subprocess.TimeoutExpired:
        raise WorktreeError(f"git {' '.join(args[:2])} timed out after {timeout:.0f}s")


def _git_ok(args: list[str], cwd: str | None = None, timeout: float = 30.0) -> str:
    r = _git(args, cwd=cwd, timeout=timeout)
    if r.returncode != 0:
        detail = (r.stderr or r.stdout).strip()
        raise WorktreeError(f"git {' '.join(args)} failed: {detail}")
    return r.stdout


# --- Repo anchoring -----------------------------------------------------------

def repo_anchor(cwd: str | None = None) -> Path:
    """Main checkout root, stable from inside any linked worktree.

    Derived from the *common* git dir so that running `start` while standing
    in another feature worktree still lands new worktrees in the main pool
    instead of nesting them under the current worktree.
    """
    r = _git(["rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=cwd, timeout=10.0)
    if r.returncode != 0:
        raise WorktreeError("not inside a git repository")
    common = Path(r.stdout.strip())
    return common.parent


def pool_root(anchor: Path) -> Path:
    return anchor.joinpath(*POOL_SEGMENTS)


def feature_path(anchor: Path, feature: str) -> Path:
    return pool_root(anchor) / feature


def validate_feature(feature: str) -> None:
    if not feature or feature.strip() != feature:
        raise WorktreeError(f"invalid feature name: {feature!r}")
    r = _git(["check-ref-format", "--branch", feature], timeout=10.0)
    if r.returncode != 0:
        raise WorktreeError(f"invalid feature name (not a valid branch name): {feature!r}")


# --- Git state reads ----------------------------------------------------------

@dataclass
class WorktreeInfo:
    path: str
    head: str = ""
    branch: str = ""  # short branch name; "" when detached/bare
    prunable: bool = False
    locked: bool = False
    is_main: bool = False


def list_worktrees(anchor: Path) -> list[WorktreeInfo]:
    out = _git_ok(["worktree", "list", "--porcelain"], cwd=str(anchor), timeout=15.0)
    items: list[WorktreeInfo] = []
    current: WorktreeInfo | None = None
    for line in out.splitlines():
        if line.startswith("worktree "):
            if current is not None:
                items.append(current)
            current = WorktreeInfo(path=line[len("worktree "):])
        elif current is None:
            continue
        elif line.startswith("HEAD "):
            current.head = line[len("HEAD "):]
        elif line.startswith("branch "):
            ref = line[len("branch "):]
            current.branch = ref[len("refs/heads/"):] if ref.startswith("refs/heads/") else ref
        elif line.startswith("prunable"):
            current.prunable = True
        elif line.startswith("locked"):
            current.locked = True
    if current is not None:
        items.append(current)
    if items:
        items[0].is_main = True
    return items


def find_feature_worktree(worktrees: list[WorktreeInfo], feature: str) -> WorktreeInfo | None:
    for wt in worktrees:
        if wt.branch == feature and not wt.is_main:
            return wt
    return None


def branch_exists(anchor: Path, feature: str) -> bool:
    r = _git(["show-ref", "--verify", "--quiet", f"refs/heads/{feature}"], cwd=str(anchor), timeout=10.0)
    return r.returncode == 0


def rev_parse(anchor: Path, ref: str) -> str:
    r = _git(["rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"], cwd=str(anchor), timeout=10.0)
    if r.returncode != 0:
        raise WorktreeError(f"cannot resolve '{ref}' to a commit")
    return r.stdout.strip()


def is_ancestor(anchor: Path, ancestor_oid: str, ref: str) -> bool:
    r = _git(["merge-base", "--is-ancestor", ancestor_oid, ref], cwd=str(anchor), timeout=10.0)
    return r.returncode == 0


def worktree_dirty(wt_path: str) -> bool:
    out = _git_ok(["status", "--porcelain", "--untracked-files=all"], cwd=wt_path, timeout=20.0)
    return bool(out.strip())


def in_progress_ops(wt_path: str) -> list[str]:
    """Names of git operations mid-flight in *wt_path* (rebase, merge, ...)."""
    args = ["rev-parse"]
    for marker, _ in _IN_PROGRESS_MARKERS:
        args.extend(["--git-path", marker])
    out = _git_ok(args, cwd=wt_path, timeout=10.0)
    paths = out.splitlines()
    ops: list[str] = []
    base = Path(wt_path)
    for (_, op), p in zip(_IN_PROGRESS_MARKERS, paths):
        candidate = Path(p) if os.path.isabs(p) else base / p
        if candidate.exists() and op not in ops:
            ops.append(op)
    return ops


# --- Branch metadata (git config branch.<feature>.hive-*) ----------------------

def read_meta(anchor: Path, feature: str) -> dict[str, str]:
    r = _git(["config", "--get-regexp", rf"^branch\.{_re_escape(feature)}\.(hive-|gh-merge-base)"], cwd=str(anchor), timeout=10.0)
    meta: dict[str, str] = {}
    if r.returncode != 0:
        return meta
    prefix = f"branch.{feature}."
    for line in r.stdout.splitlines():
        key, _, value = line.partition(" ")
        if key.startswith(prefix):
            meta[key[len(prefix):]] = value
    return meta


def _re_escape(s: str) -> str:
    import re
    return re.escape(s)


def write_meta(anchor: Path, feature: str, meta: dict[str, str]) -> None:
    for key, value in meta.items():
        _git_ok(["config", f"branch.{feature}.{key}", value], cwd=str(anchor), timeout=10.0)


def clear_meta(anchor: Path, feature: str) -> list[str]:
    """Remove hive-* keys for *feature*; keep gh-merge-base (the branch and its
    PR remain alive after done — gh still resolves base from it)."""
    cleared: list[str] = []
    existing = read_meta(anchor, feature)
    for key in META_KEYS:
        if key in existing:
            _git(["config", "--unset", f"branch.{feature}.{key}"], cwd=str(anchor), timeout=10.0)
            cleared.append(key)
    return cleared


def hive_labeled_branches(anchor: Path) -> list[str]:
    r = _git(["config", "--get-regexp", r"^branch\..*\.hive-owner$"], cwd=str(anchor), timeout=10.0)
    if r.returncode != 0:
        return []
    branches = []
    for line in r.stdout.splitlines():
        key = line.split(" ", 1)[0]
        # branch.<name>.hive-owner — <name> may itself contain dots/slashes.
        name = key[len("branch."):-len(".hive-owner")]
        if name:
            branches.append(name)
    return branches


# --- Base resolution -----------------------------------------------------------

@dataclass
class BaseResolution:
    ref: str
    oid: str
    source: str  # explicit | squad-integration | default-branch


def resolve_base(anchor: Path, explicit: str | None, squad_integration: str | None) -> BaseResolution:
    """Resolve the base ref for a new feature.

    *squad_integration* is None outside squad context; inside squad context it is
    the integration branch from the window option ("" when unset, which is a
    hard failure — base must never silently fall back to the default branch in
    a squad, or sub-PRs aim at main).
    """
    if explicit:
        return BaseResolution(ref=explicit, oid=rev_parse(anchor, explicit), source="explicit")
    if squad_integration is not None:
        if not squad_integration:
            raise WorktreeError(
                "squad context but no integration branch is set "
                "(@hive-squad-integration-branch); pass --base <integration> explicitly"
            )
        return BaseResolution(
            ref=squad_integration, oid=rev_parse(anchor, squad_integration), source="squad-integration"
        )
    r = _git(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"], cwd=str(anchor), timeout=10.0)
    default = r.stdout.strip() if r.returncode == 0 else ""
    if not default:
        raise WorktreeError(
            "cannot detect the default branch (no origin/HEAD — repo without a "
            "remote, or origin/HEAD unset); pass --base <ref> explicitly"
        )
    return BaseResolution(ref=default, oid=rev_parse(anchor, default), source="default-branch")


# --- start ----------------------------------------------------------------------

@dataclass
class StartResult:
    feature: str
    branch: str
    path: str
    mode: str  # created | existing | attached | adopted-existing-branch | needs-rebase
    owner: str
    team: str
    squad_name: str
    base: str
    base_oid: str
    worktree_root: str
    git_common_dir: str
    warnings: list[str] = field(default_factory=list)

    @property
    def ready(self) -> bool:
        return self.mode in READY_MODES

    def to_json(self) -> dict:
        return {
            "feature": self.feature,
            "branch": self.branch,
            "path": self.path,
            "mode": self.mode,
            "owner": self.owner,
            "team": self.team,
            "squadName": self.squad_name,
            "base": self.base,
            "baseOid": self.base_oid,
            "worktreeRoot": self.worktree_root,
            "gitCommonDir": self.git_common_dir,
            "warnings": list(self.warnings),
        }


def start(
    anchor: Path,
    feature: str,
    *,
    base: BaseResolution,
    owner: str,
    team: str = "",
    squad_name: str = "",
    gh_merge_base: str | None = None,
    now: float | None = None,
) -> StartResult:
    validate_feature(feature)
    expected = feature_path(anchor, feature)
    common_dir = _git_ok(["rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=str(anchor), timeout=10.0).strip()

    def result(mode: str, path: str = "", warnings: list[str] | None = None) -> StartResult:
        return StartResult(
            feature=feature,
            branch=feature,
            path=path or str(expected),
            mode=mode,
            owner=owner,
            team=team,
            squad_name=squad_name,
            base=base.ref,
            base_oid=base.oid,
            worktree_root=str(pool_root(anchor)),
            git_common_dir=common_dir,
            warnings=warnings or [],
        )

    def required_meta() -> dict[str, str]:
        meta = {
            "hive-owner": owner,
            "hive-team": team or owner,
            "hive-base": base.ref,
            "hive-base-oid": base.oid,
            "hive-created": str(now if now is not None else time.time()),
        }
        if squad_name:
            meta["hive-squad"] = squad_name
        if gh_merge_base:
            meta[GH_MERGE_BASE_KEY] = gh_merge_base
        return meta

    def sync_ready_meta(existing: dict[str, str]) -> None:
        """Every ready start must leave the full required config current —
        notably gh-merge-base when the squad's integration branch moved.
        The first-created timestamp is the only key that survives as-is."""
        fresh = required_meta()
        if "hive-created" in existing:
            fresh["hive-created"] = existing["hive-created"]
        delta = {k: v for k, v in fresh.items() if existing.get(k) != v}
        if delta:
            write_meta(anchor, feature, delta)

    worktrees = list_worktrees(anchor)
    # Case-D detection must see every checkout of the branch, the main
    # checkout included — only the expected pool path may proceed.
    checkout = next((w for w in worktrees if w.branch == feature), None)
    if checkout is not None and (checkout.prunable or not Path(checkout.path).exists()):
        # Stale registration (directory manually removed): prune, then treat
        # the feature as branch-only.
        _git_ok(["worktree", "prune"], cwd=str(anchor), timeout=15.0)
        checkout = None
    if checkout is not None and Path(checkout.path).resolve() != expected.resolve():
        raise WorktreeError(
            f"branch '{feature}' is already checked out at {checkout.path}; "
            "free it there or pick another feature name"
        )
    wt = checkout

    has_branch = branch_exists(anchor, feature)
    meta = read_meta(anchor, feature) if has_branch else {}
    stored_owner = meta.get("hive-owner", "")
    foreign = bool(stored_owner) and stored_owner != owner

    if wt is not None:
        # Case B: worktree exists at the pool path.
        if foreign:
            raise WorktreeError(
                f"feature '{feature}' is owned by '{stored_owner}' (current: '{owner}'); "
                "pick another feature name or have the owner release it"
            )
        if not is_ancestor(anchor, base.oid, feature):
            return result(
                "needs-rebase",
                path=wt.path,
                warnings=[
                    f"branch does not contain resolved base {base.ref} ({base.oid[:12]}); "
                    "rebase onto it, then rerun start"
                ],
            )
        if not stored_owner:
            write_meta(anchor, feature, required_meta())
            return result("adopted-existing-branch", path=wt.path)
        sync_ready_meta(meta)
        return result("existing", path=wt.path)

    if has_branch:
        # Case C: branch exists, no worktree (post-done rework is the common
        # path here — done keeps the branch for the PR's lifetime).
        if foreign:
            raise WorktreeError(
                f"feature '{feature}' is owned by '{stored_owner}' (current: '{owner}'); "
                "pick another feature name or have the owner release it"
            )
        compatible = is_ancestor(anchor, base.oid, feature)
        expected.parent.mkdir(parents=True, exist_ok=True)
        _git_ok(["worktree", "add", str(expected), feature], cwd=str(anchor), timeout=120.0)
        if not compatible:
            return result(
                "needs-rebase",
                warnings=[
                    f"branch does not contain resolved base {base.ref} ({base.oid[:12]}); "
                    "worktree attached so you can rebase, then rerun start"
                ],
            )
        if not stored_owner:
            write_meta(anchor, feature, required_meta())
            return result("adopted-existing-branch")
        sync_ready_meta(meta)
        return result("attached")

    # Case A: nothing exists yet.
    expected.parent.mkdir(parents=True, exist_ok=True)
    _git_ok(["worktree", "add", "-b", feature, str(expected), base.oid], cwd=str(anchor), timeout=120.0)
    write_meta(anchor, feature, required_meta())
    return result("created")


# --- done -----------------------------------------------------------------------

@dataclass
class DoneResult:
    feature: str
    branch: str
    removed_path: str
    branch_kept: bool
    cleared_config_keys: list[str]
    forced: bool
    status_summary: str = ""
    warnings: list[str] = field(default_factory=list)

    def to_json(self) -> dict:
        return {
            "feature": self.feature,
            "branch": self.branch,
            "removedPath": self.removed_path,
            "branchKept": self.branch_kept,
            "clearedConfigKeys": list(self.cleared_config_keys),
            "forced": self.forced,
            "statusSummary": self.status_summary,
            "warnings": list(self.warnings),
        }


def done(anchor: Path, feature: str, *, force: bool = False, caller_cwd: str = "") -> DoneResult:
    validate_feature(feature)
    worktrees = list_worktrees(anchor)
    wt = find_feature_worktree(worktrees, feature)
    if wt is None:
        raise WorktreeError(
            f"no worktree found for feature '{feature}' "
            "(see `hive worktree status` — done only removes worktrees, never branches)"
        )

    wt_path = Path(wt.path).resolve()
    if caller_cwd:
        try:
            Path(caller_cwd).resolve().relative_to(wt_path)
            raise WorktreeError(
                f"you are inside {wt.path}; leave the worktree first "
                "(ExitWorktree action=keep, or cd back to the main checkout), then rerun done"
            )
        except ValueError:
            pass

    summary = ""
    stale = not wt_path.exists()
    if not stale:
        ops = in_progress_ops(wt.path)
        dirty = worktree_dirty(wt.path)
        if not force:
            if ops:
                raise WorktreeError(
                    f"git {'/'.join(ops)} in progress in {wt.path}; "
                    "finish or abort it, or rerun with --force to discard"
                )
            if dirty:
                raise WorktreeError(
                    f"worktree {wt.path} has uncommitted changes; commit them, "
                    "or rerun with --force to discard them (destructive)"
                )
        else:
            # --force always reports what it is about to discard — a clean
            # tree still gets the summary so the abandon decision is auditable.
            summary = _git_ok(
                ["status", "--short", "--branch", "--untracked-files=all"], cwd=wt.path, timeout=20.0
            ).rstrip()
            summary += "\n(ignored files are not included in this summary)"

    remove_args = ["worktree", "remove"]
    if force:
        # Twice: git requires double --force for dirty + locked combinations.
        remove_args.extend(["--force", "--force"])
    remove_args.append(wt.path)
    _git_ok(remove_args, cwd=str(anchor), timeout=60.0)

    cleared = clear_meta(anchor, feature)
    return DoneResult(
        feature=feature,
        branch=feature,
        removed_path=wt.path,
        branch_kept=True,
        cleared_config_keys=cleared,
        forced=force,
        status_summary=summary,
    )


# --- status ----------------------------------------------------------------------

def feature_status(anchor: Path, feature: str) -> dict:
    validate_feature(feature)
    worktrees = list_worktrees(anchor)
    wt = find_feature_worktree(worktrees, feature)
    has_branch = branch_exists(anchor, feature)
    meta = read_meta(anchor, feature)

    current_base_oid = ""
    base_ref = meta.get("hive-base", "")
    if base_ref:
        try:
            current_base_oid = rev_parse(anchor, base_ref)
        except WorktreeError:
            current_base_oid = ""

    stale = bool(wt and (wt.prunable or not Path(wt.path).exists()))
    dirty = False
    ops: list[str] = []
    if wt and not stale:
        dirty = worktree_dirty(wt.path)
        ops = in_progress_ops(wt.path)

    warnings: list[str] = []
    if not has_branch:
        state = "unknown-branch"
    elif stale:
        state = "stale"
        warnings.append("worktree directory is gone; `git worktree prune` (or rerun start) will clean the entry")
    elif wt is None:
        state = "branch-only"
    elif current_base_oid and not is_ancestor(anchor, current_base_oid, feature):
        state = "needs-rebase"
    else:
        state = "active"

    return {
        "feature": feature,
        "branchExists": has_branch,
        "worktreePath": wt.path if wt else "",
        "owner": meta.get("hive-owner", ""),
        "base": base_ref,
        "baseOid": meta.get("hive-base-oid", ""),
        "currentBaseOid": current_base_oid,
        "state": state,
        "dirty": dirty,
        "inProgress": ops,
        "stale": stale,
        "warnings": warnings,
    }


def pool_status(anchor: Path) -> list[dict]:
    features: list[str] = []
    seen: set[str] = set()
    for wt in list_worktrees(anchor):
        if wt.is_main or not wt.branch:
            continue
        if wt.branch not in seen:
            seen.add(wt.branch)
            features.append(wt.branch)
    for branch in hive_labeled_branches(anchor):
        if branch not in seen:
            seen.add(branch)
            features.append(branch)
    return [feature_status(anchor, f) for f in features]
