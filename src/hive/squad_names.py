"""Squad instance naming — pool of short, memorable names used as the
public namespace for a squad.

Each `hive squad init` picks one name from ``SQUAD_NAME_POOL`` that is not
currently claimed by any live `@hive-group` tag across the tmux server.
The picked name then appears as:

  - `@hive-group=<name>` on every pane in the squad
  - `@hive-agent=<name>.orch / <name>.challenger`
  - `@hive-agent=<name>.worker-<N> / <name>.validator-<N>` for peers
  - `@hive-owner=<name>.orch` on spawned peers

This lets multiple squads coexist in the same tmux session (or across
sessions) without collision in qualified-name lookup.
"""
from __future__ import annotations

import re

from . import tmux


SQUAD_NAME_POOL: tuple[str, ...] = (
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
    """Return ``(ok, reason)`` for a caller-supplied squad name.

    Rules: 1-16 chars, lowercase ASCII letters/digits/dashes only,
    must start with a letter. The bare tokens ``squad`` (topology word),
    ``crew`` (pre-rename legacy scheme) and ``ccd`` (the send address of
    Claude sessions outside any team — a squad named ccd would make
    ``ccd.orch`` ambiguous) are reserved, never instance names.
    """
    if not name:
        return False, "squad name cannot be empty"
    if name in ("squad", "crew", "ccd"):
        return False, f"'{name}' is reserved; pick a distinct instance name"
    if not _NAME_RE.match(name):
        return False, (
            "squad name must be 1-16 lowercase ASCII chars "
            "(letters/digits/dashes, starting with a letter)"
        )
    return True, ""


_FEATURE_ID_RE = re.compile(r"^[a-z][a-z0-9]*(-[a-z0-9]+){0,3}$")
_FEATURE_ID_HINT = (
    "feature id becomes the branch / worktree / window / sub-PR name: "
    "semantic kebab-case, lowercase, ≤4 dash-separated words, ≤32 chars, "
    "named for what it does (e.g. 'contract-usd-amount-words'); "
    "step/sequence ids like 'F2-03_04' belong in features.json fields, not the name"
)


def validate_feature_id(feature_id: str) -> tuple[bool, str]:
    """Return ``(ok, reason)`` for a spawn-duo feature id.

    Step-number shapes (``F2``, ``03`` …) encode ordering, not meaning —
    rejected even in lowercase kebab form (``f2-03-04``).
    """
    if not feature_id:
        return False, f"feature id cannot be empty — {_FEATURE_ID_HINT}"
    if len(feature_id) > 32:
        return False, f"feature id is too long ({len(feature_id)} > 32 chars) — {_FEATURE_ID_HINT}"
    if not _FEATURE_ID_RE.match(feature_id):
        return False, f"feature id '{feature_id}' is not semantic kebab-case — {_FEATURE_ID_HINT}"
    for segment in feature_id.split("-"):
        if segment.isdigit() or re.fullmatch(r"f\d+", segment):
            return False, (
                f"feature id '{feature_id}' has step/sequence segment '{segment}' — {_FEATURE_ID_HINT}"
            )
    return True, ""


def claimed_names() -> set[str]:
    """Return every squad name currently claimed by a live ``@hive-group``
    tag **or** a qualified ``@hive-agent`` prefix across the tmux server.

    A pane with ``@hive-agent=krays.coco`` claims ``krays`` even when
    ``@hive-group`` is missing — the qualified resolver can route to it,
    so the namespace must be reserved.

    Filters out the empty string and the reserved tokens: ``squad`` and
    the pre-rename legacy ``crew``, which may still sit on stale panes —
    neither is ever a valid instance name to collide with.
    """
    claimed: set[str] = set()
    for pane in tmux.list_panes_all():
        group = (pane.group or "").strip()
        if group and group not in ("squad", "crew"):
            claimed.add(group)
        agent = (pane.agent or "").strip()
        if "." in agent:
            prefix, _, _ = agent.partition(".")
            ok, _ = validate_name(prefix)
            if ok:
                claimed.add(prefix)
    return claimed


def pick_available_name(fallback_suffix: str = "") -> str:
    """Pick a pool name not currently claimed by any live squad.

    Scans the entire tmux server (qualified-name resolution is
    server-wide, so names must be globally unique). Falls back to
    ``squad-<fallback_suffix>`` when every pool name is taken — caller
    should pass a stable disambiguator (e.g. tmux window_id stripped of
    the leading ``@``).
    """
    used = claimed_names()
    for candidate in SQUAD_NAME_POOL:
        if candidate not in used:
            return candidate
    suffix = fallback_suffix.lstrip("@") or "0"
    fallback = f"squad-{suffix}"
    counter = 0
    while fallback in used:
        counter += 1
        fallback = f"squad-{suffix}-{counter}"
    return fallback


def pick_range_base(squad_name: str, claimed_bases: set[int]) -> int:
    """Pick a 1000-step tmux window-index base for *squad_name*.

    Each squad owns a 1000-wide slice of peer window indices so peer
    windows sort visually by squad (peaky 1000-1999, krays 2000-2999,
    crips 3000-3999, ...). Pool names get deterministic bases by pool
    position (peaky → 1000, krays → 2000, ...); anything else falls
    back to the first unused 1000-multiple.
    """
    if squad_name in SQUAD_NAME_POOL:
        preferred = (SQUAD_NAME_POOL.index(squad_name) + 1) * 1000
        if preferred not in claimed_bases:
            return preferred
    base = 1000
    while base in claimed_bases:
        base += 1000
    return base
