"""Durable team snapshots backing `hive ls` and `hive resume`.

A team's identity normally lives only in tmux options and dies with the
server. The snapshot store keeps one JSON file per team handle under
``$HIVE_HOME/state/resume/`` so a dead team (tmux restart, reboot) can be
listed and rebuilt with each member's original agent session.

The store is deliberately dumb: callers (sidecar writer, duo init, `hive
resume`) compute the payload; this module owns schema validation, safe
file naming, atomic writes, roster merging, change detection, and the
one-predecessor archive on a new team instance.
"""

from __future__ import annotations

import json
import os
import re
import tempfile
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1

_HANDLE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_PREV_SUFFIX = ".prev"

_MEMBER_FIELDS = ("name", "cli", "model", "sessionId", "cwd")


def repo_label(cwd: str) -> str:
    """Human repo name for *cwd*: the main checkout's basename.

    A linked worktree's own directory is named after the feature, which is
    exactly what a "repo" column must not show — resolve through the git
    common dir instead. Non-git (or vanished) paths fall back to the cwd
    basename; empty stays empty.
    """
    if not cwd:
        return ""
    try:
        from .worktree import repo_anchor

        return repo_anchor(cwd).name
    except Exception:
        return os.path.basename(cwd.rstrip("/"))


def age(saved_at: str, *, now: float | None = None) -> str:
    """Relative rendering of a UTC ``savedAt`` stamp ("2h ago").

    Bad/empty/future input degrades safely (never raises): empty → "?",
    unparseable → the raw value, future → "just now". Timezone-independent —
    the stamp is explicit UTC and *now* is epoch seconds.
    """
    if not saved_at:
        return "?"
    import datetime as _dt
    import time as _time

    try:
        stamp = _dt.datetime.strptime(saved_at, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=_dt.timezone.utc
        )
    except (ValueError, TypeError):
        return saved_at
    delta = (now if now is not None else _time.time()) - stamp.timestamp()
    if delta < 60:
        return "just now"
    if delta < 3600:
        return f"{int(delta // 60)}m ago"
    if delta < 86400:
        return f"{int(delta // 3600)}h ago"
    return f"{int(delta // 86400)}d ago"


def git_branch(cwd: str) -> str:
    """Current git branch of *cwd*, or "" (not a repo / detached / no cwd)."""
    if not cwd:
        return ""
    import subprocess

    try:
        r = subprocess.run(
            ["git", "-C", cwd, "branch", "--show-current"],
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return r.stdout.strip() if r.returncode == 0 else ""


def short_id(snap: dict[str, Any]) -> str:
    """4-char typeable id for a snapshot, derived — never stored.

    Stable per team instance (handle + createdAt), so `hive ls` and
    `hive resume` always agree, and a new same-name instance gets a new id
    with no schema change or migration.
    """
    import hashlib

    seed = f"{snap.get('handle', '')}:{snap.get('createdAt', '')}"
    return hashlib.sha256(seed.encode()).hexdigest()[:4]


def store_dir() -> Path:
    home = Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))
    return home / "state" / "resume"


def safe_handle(handle: str) -> str | None:
    """*handle* as a file stem, or None when it could escape the store."""
    if not handle or not _HANDLE_RE.match(handle) or ".." in handle:
        return None
    return handle


def is_archive_handle(handle: str) -> bool:
    """True for the reserved ``<team>.prev`` predecessor-archive namespace.

    Archive handles stay loadable and listable, but they are never a valid
    primary team handle: a real team named ``foo.prev`` would collide with
    ``foo``'s archive slot and silently lose one of the two snapshots.
    """
    return handle.endswith(_PREV_SUFFIX)


def snapshot_path(handle: str) -> Path | None:
    stem = safe_handle(handle)
    if stem is None:
        return None
    return store_dir() / f"{stem}.json"


def build_snapshot(
    *,
    handle: str,
    team: str,
    group: str,
    window_name: str,
    workspace: str,
    repo_cwd: str,
    branch: str,
    created_at: str,
    members: list[dict[str, str]],
    repo: str = "",
    pr: str = "",
) -> dict[str, Any]:
    return {
        "schema": SCHEMA_VERSION,
        "handle": handle,
        "team": team,
        "group": group,
        "windowName": window_name,
        "workspace": workspace,
        "repoCwd": repo_cwd,
        "repo": repo,
        "branch": branch,
        "pr": pr,
        "createdAt": created_at,
        "savedAt": "",
        "members": [
            {field: str(m.get(field, "") or "") for field in _MEMBER_FIELDS}
            for m in members
        ],
    }


def _valid(snap: Any) -> bool:
    if not isinstance(snap, dict) or snap.get("schema") != SCHEMA_VERSION:
        return False
    if not snap.get("handle") or not snap.get("team"):
        return False
    members = snap.get("members")
    if not isinstance(members, list):
        return False
    return all(isinstance(m, dict) and m.get("name") for m in members)


def load_snapshot(handle: str) -> dict[str, Any] | None:
    """The valid snapshot for *handle*, or None (missing/corrupt/unsafe)."""
    path = snapshot_path(handle)
    if path is None or not path.is_file():
        return None
    try:
        snap = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return snap if _valid(snap) else None


def list_snapshots() -> list[dict[str, Any]]:
    """Every stored snapshot; unreadable files surface as corrupt markers."""
    root = store_dir()
    if not root.is_dir():
        return []
    out: list[dict[str, Any]] = []
    for path in sorted(root.glob("*.json")):
        handle = path.stem
        if safe_handle(handle) is None:
            continue
        try:
            snap = json.loads(path.read_text())
        except (OSError, ValueError):
            snap = None
        if snap is not None and _valid(snap):
            out.append(snap)
        else:
            out.append({"handle": handle, "corrupt": True})
    return out


def merge_members(
    existing: list[dict[str, str]], observed: list[dict[str, str]]
) -> list[dict[str, str]]:
    """Update *existing* roster from *observed* without ever dropping members.

    A pane that disappeared stops being observed, but its member entry (and
    sessionId — the thing `hive resume` exists to bring back) must survive.
    Observed non-empty fields win; empty observations never erase state.
    """
    by_name: dict[str, dict[str, str]] = {
        str(m.get("name")): {field: str(m.get(field, "") or "") for field in _MEMBER_FIELDS}
        for m in existing
        if m.get("name")
    }
    for obs in observed:
        name = str(obs.get("name") or "")
        if not name:
            continue
        entry = by_name.setdefault(name, {field: "" for field in _MEMBER_FIELDS})
        entry["name"] = name
        for field in ("cli", "model", "sessionId", "cwd"):
            value = str(obs.get(field, "") or "")
            if value:
                entry[field] = value
    return list(by_name.values())


def _payload_for_compare(snap: dict[str, Any]) -> str:
    trimmed = {k: v for k, v in snap.items() if k != "savedAt"}
    return json.dumps(trimmed, ensure_ascii=False, sort_keys=True)


def save_snapshot(
    snap: dict[str, Any], *, now: str, archive_on_new_instance: bool = True
) -> str:
    """Persist *snap*; returns ``written`` / ``unchanged`` / ``rejected``.

    ``savedAt`` is stamped only when the effective payload changed, so
    steady-state sidecar ticks never touch the file. When the stored file
    belongs to a different team instance (``createdAt`` mismatch) it is
    archived to the single ``<handle>.prev`` slot first; `hive resume`
    passes ``archive_on_new_instance=False`` because a restored team is a
    continuation of the same logical team, not a new one.
    """
    if not _valid(snap):
        return "rejected"
    handle = str(snap["handle"])
    if is_archive_handle(handle):
        return "rejected"  # the .prev namespace is written only by archiving
    path = snapshot_path(handle)
    if path is None:
        return "rejected"

    existing: dict[str, Any] | None = None
    if path.is_file():
        try:
            candidate = json.loads(path.read_text())
        except (OSError, ValueError):
            candidate = None
        if candidate is not None and _valid(candidate):
            existing = candidate

    if existing is not None and _payload_for_compare(existing) == _payload_for_compare(snap):
        return "unchanged"

    if (
        existing is not None
        and archive_on_new_instance
        and str(existing.get("createdAt")) != str(snap.get("createdAt"))
    ):
        prev_stem = f"{path.stem}{_PREV_SUFFIX}"
        prev = dict(existing)
        prev["handle"] = prev_stem
        _write_atomic(path.with_name(f"{prev_stem}.json"), prev)

    stamped = dict(snap)
    stamped["savedAt"] = now
    _write_atomic(path, stamped)
    return "written"


def delete_snapshot(handle: str) -> bool:
    path = snapshot_path(handle)
    if path is None:
        return False
    try:
        path.unlink()
        return True
    except OSError:
        return False


def _write_atomic(path: Path, snap: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".snap.", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w") as fh:
            json.dump(snap, fh, ensure_ascii=False, indent=2, sort_keys=True)
            fh.write("\n")
        os.replace(tmp, path)
    except OSError:
        try:
            os.unlink(tmp)
        except OSError:
            pass
