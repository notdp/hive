"""The team registry: durable team truth under ``$HIVE_HOME/state/teams/``.

One JSON file per team. This store is the authoritative record of a team's
identity and roster — tmux windows and panes are a display layer resolved on
top of it, so a team survives a killed window or a tmux restart.

Write lanes are split by authority:

- **Roster membership belongs to the CLI**: :func:`record_team`,
  :func:`record_member`, :func:`remove_member` and :func:`delete_team` add
  and remove state at create/spawn/kill/delete time, under the store lock.
- **The sidecar only backfills**: :func:`backfill_members` refreshes fields
  of names already in the roster and never adds one — an observation racing
  a kill must not resurrect the killed member.

Schema (deliberately minimal): ``team``, ``workspace``, ``createdAt``
(instance identity — a recycled name is a new instance), ``display`` (the
tmux window id currently rendering the team; a cache, never authority), and
``members`` rows of ``name`` / ``cli`` / ``model`` / ``sessionId`` (the
engine identity: claude jobId, codex threadId, grok session id) / ``cwd``.
"""

from __future__ import annotations

import fcntl
import json
import os
import re
import tempfile
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator

_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")

MEMBER_FIELDS = ("name", "cli", "model", "sessionId", "cwd")


def store_dir() -> Path:
    home = Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))
    return home / "state" / "teams"


def entry_path(team: str) -> Path | None:
    """The team's registry file, or None when the name could escape the store."""
    if not team or not _NAME_RE.match(team) or ".." in team:
        return None
    return store_dir() / f"{team}.json"


def _valid(entry: Any) -> bool:
    if not isinstance(entry, dict) or not entry.get("team"):
        return False
    members = entry.get("members")
    if not isinstance(members, list):
        return False
    return all(isinstance(m, dict) and m.get("name") for m in members)


def load(team: str) -> dict[str, Any] | None:
    """The valid registry entry for *team*, or None (missing/corrupt/unsafe)."""
    path = entry_path(team)
    if path is None or not path.is_file():
        return None
    try:
        entry = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return entry if _valid(entry) else None


def list_entries() -> list[dict[str, Any]]:
    """Every valid registry entry; unreadable files surface as corrupt markers."""
    root = store_dir()
    if not root.is_dir():
        return []
    out: list[dict[str, Any]] = []
    for path in sorted(root.glob("*.json")):
        if entry_path(path.stem) is None:
            continue
        try:
            entry = json.loads(path.read_text())
        except (OSError, ValueError):
            entry = None
        if entry is not None and _valid(entry):
            out.append(entry)
        else:
            out.append({"team": path.stem, "corrupt": True})
    return out


@contextmanager
def locked() -> Iterator[None]:
    """Exclusive store lock for read-merge-write cycles.

    One fcntl lock for the whole store: writers are a handful of CLI calls
    and one sidecar tick per team every 30s, so contention is nil and a
    single lock keeps the kill-vs-backfill race closed by construction.
    """
    root = store_dir()
    root.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(root / ".lock"), os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def _member_row(member: dict[str, str]) -> dict[str, str]:
    return {field: str(member.get(field, "") or "") for field in MEMBER_FIELDS}


def record_team(
    *,
    team: str,
    workspace: str,
    created_at: str,
    members: list[dict[str, str]] | None = None,
    display: str = "",
) -> str:
    """Register a team at creation time (CLI write lane), overwriting any
    predecessor a recycled name left behind. Returns ``written``/``rejected``."""
    path = entry_path(team)
    if path is None:
        return "rejected"
    entry = {
        "team": team,
        "workspace": workspace,
        "createdAt": created_at,
        "display": display,
        "members": [_member_row(m) for m in (members or []) if m.get("name")],
    }
    with locked():
        _write_atomic(path, entry)
    return "written"


def record_member(
    team: str, member: dict[str, str], *, created_at: str = ""
) -> str:
    """Add or replace one member row in the team's roster (CLI write lane).

    *created_at*, when given, must match the stored instance — a stale entry
    left by a recycled name is never edited into (returns ``missing`` so the
    caller can seed a fresh entry).
    """
    name = str(member.get("name") or "")
    if not name:
        return "rejected"
    path = entry_path(team)
    if path is None:
        return "rejected"
    with locked():
        entry = load(team)
        if entry is None or (created_at and str(entry.get("createdAt")) != created_at):
            return "missing"
        entry["members"] = [
            m for m in entry.get("members", []) if m.get("name") != name
        ] + [_member_row(member)]
        _write_atomic(path, entry)
    return "written"


def remove_member(team: str, name: str, *, created_at: str = "") -> str:
    """Drop one member row from the team's roster (CLI write lane)."""
    path = entry_path(team)
    if path is None:
        return "rejected"
    with locked():
        entry = load(team)
        if entry is None or (created_at and str(entry.get("createdAt")) != created_at):
            return "missing"
        entry["members"] = [m for m in entry.get("members", []) if m.get("name") != name]
        _write_atomic(path, entry)
    return "written"


def set_display(team: str, display: str) -> str:
    """Update the display cache (the tmux window id rendering the team)."""
    path = entry_path(team)
    if path is None:
        return "rejected"
    with locked():
        entry = load(team)
        if entry is None:
            return "missing"
        if entry.get("display") == display:
            return "unchanged"
        entry["display"] = display
        _write_atomic(path, entry)
    return "written"


def delete_team(team: str) -> None:
    """Remove the team's registry entry (delete is the team's end of life)."""
    path = entry_path(team)
    if path is None:
        return
    with locked():
        try:
            path.unlink()
        except OSError:
            pass


def backfill_members(
    existing: list[dict[str, str]], observed: list[dict[str, str]]
) -> list[dict[str, str]]:
    """Refresh fields of members already in *existing* from *observed*.

    The sidecar's write lane: observation updates what a known member looks
    like (model switch, cwd change, a sessionId learned late) but never adds
    or removes a name — roster membership belongs to the CLI writers, and an
    observation racing a `hive kill` must not resurrect the killed member.
    Observed non-empty fields win; empty observations never erase state.
    """
    by_name: dict[str, dict[str, str]] = {
        str(m.get("name")): _member_row(m) for m in existing if m.get("name")
    }
    for obs in observed:
        entry = by_name.get(str(obs.get("name") or ""))
        if entry is None:
            continue
        for field in ("cli", "model", "sessionId", "cwd"):
            value = str(obs.get(field, "") or "")
            if value:
                entry[field] = value
    return list(by_name.values())


def backfill(
    team: str,
    observed: list[dict[str, str]],
    *,
    created_at: str,
    display: str = "",
    workspace: str = "",
) -> str:
    """The sidecar's whole read-merge-write, under the store lock.

    Refuses a missing entry (the CLI writer owns creation) and a
    foreign-instance entry (a recycled name's predecessor must not be
    overwritten from observation). Returns ``written``/``missing``/``unchanged``.
    """
    path = entry_path(team)
    if path is None:
        return "missing"
    with locked():
        entry = load(team)
        if entry is None or str(entry.get("createdAt")) != created_at:
            return "missing"
        updated = dict(entry)
        updated["members"] = backfill_members(
            list(entry.get("members", [])), observed
        )
        if display:
            updated["display"] = display
        if workspace:
            updated["workspace"] = workspace
        if updated == entry:
            return "unchanged"
        _write_atomic(path, updated)
    return "written"


def _write_atomic(path: Path, entry: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".reg.", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w") as fh:
            json.dump(entry, fh, ensure_ascii=False, indent=2, sort_keys=True)
            fh.write("\n")
        os.replace(tmp, path)
    except OSError:
        try:
            os.unlink(tmp)
        except OSError:
            pass
