"""Gang instance naming — pool of short, memorable names used as the
public namespace for a gang squad.

Each `hive gang init` picks one name from ``GANG_NAME_POOL`` that is not
currently claimed by any live `@hive-group` tag across the tmux server.
The picked name then appears as:

  - `@hive-group=<name>` on every pane in the gang
  - `@hive-agent=<name>.orch / <name>.skeptic`
  - `@hive-agent=<name>.worker-<N> / <name>.validator-<N>` for peers
  - `@hive-owner=<name>.orch` on spawned peers

This lets multiple gangs coexist in the same tmux session (or across
sessions) without collision in qualified-name lookup.
"""
from __future__ import annotations

import re

from . import tmux


GANG_NAME_POOL: tuple[str, ...] = (
    "peaky",
    "krays",
    "crips",
    "jesse",
    "triad",
    "shelby",
    "yakuza",
    "bloods",
    "dalton",
    "bratva",
)


_NAME_RE = re.compile(r"^[a-z][a-z0-9-]{0,15}$")


def validate_name(name: str) -> tuple[bool, str]:
    """Return ``(ok, reason)`` for a caller-supplied gang name.

    Rules: 1-16 chars, lowercase ASCII letters/digits/dashes only,
    must start with a letter. Reserving the bare token ``gang`` avoids
    confusion with the legacy fixed-name scheme.
    """
    if not name:
        return False, "gang name cannot be empty"
    if name == "gang":
        return False, "'gang' is reserved; pick a distinct instance name"
    if not _NAME_RE.match(name):
        return False, (
            "gang name must be 1-16 lowercase ASCII chars "
            "(letters/digits/dashes, starting with a letter)"
        )
    return True, ""


def claimed_names() -> set[str]:
    """Return every gang name currently claimed by a live `@hive-group`
    tag across the tmux server.

    Filters out the empty string and the legacy token ``gang`` (which
    should no longer be used post-migration but may appear on stale
    panes).
    """
    claimed: set[str] = set()
    for pane in tmux.list_panes_all():
        group = (pane.group or "").strip()
        if not group or group == "gang":
            continue
        claimed.add(group)
    return claimed


def pick_available_name(fallback_suffix: str = "") -> str:
    """Pick a pool name not currently claimed by any live gang.

    Scans the entire tmux server (qualified-name resolution is
    server-wide, so names must be globally unique). Falls back to
    ``gang-<fallback_suffix>`` when every pool name is taken — caller
    should pass a stable disambiguator (e.g. tmux window_id stripped of
    the leading ``@``).
    """
    used = claimed_names()
    for candidate in GANG_NAME_POOL:
        if candidate not in used:
            return candidate
    suffix = fallback_suffix.lstrip("@") or "0"
    fallback = f"gang-{suffix}"
    counter = 0
    while fallback in used:
        counter += 1
        fallback = f"gang-{suffix}-{counter}"
    return fallback


def pick_range_base(gang_name: str, claimed_bases: set[int]) -> int:
    """Pick a 1000-step tmux window-index base for *gang_name*.

    Each gang owns a 1000-wide slice of peer window indices so peer
    windows sort visually by gang (peaky 1000-1999, krays 2000-2999,
    crips 3000-3999, ...). Pool names get deterministic bases by pool
    position (peaky → 1000, krays → 2000, ...); anything else falls
    back to the first unused 1000-multiple.
    """
    if gang_name in GANG_NAME_POOL:
        preferred = (GANG_NAME_POOL.index(gang_name) + 1) * 1000
        if preferred not in claimed_bases:
            return preferred
    base = 1000
    while base in claimed_bases:
        base += 1000
    return base
