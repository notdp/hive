"""Adaptive tmux layout for hive teams.

Picks a preset from the window's aspect ratio (tmux cell ≈ 1:2 pixel,
so char-width >= 2*char-height ≈ landscape pixels) and current pane count.
Used by Team.spawn, hive kill, hive layout, and resume.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from . import tmux


LANDSCAPE_PRESET = "main-vertical"
PORTRAIT_PRESET = "even-vertical"
MAIN_PANE_FRACTION = "50%"
# From this many panes, a main pane plus a single strip squeezes members
# into slivers — tile instead.
TILED_THRESHOLD = 5


@dataclass(frozen=True)
class LayoutChoice:
    orientation: str
    preset: str
    options: dict[str, str] = field(default_factory=dict)


def _is_landscape(width: int, height: int) -> bool:
    if width <= 0 or height <= 0:
        return True
    return width >= 2 * height


def pick(window_size: tuple[int, int], pane_count: int) -> LayoutChoice | None:
    """Return the layout choice for this window, or None when no apply should happen."""
    if pane_count < 2:
        return None
    w, h = window_size
    orientation = "horizontal" if _is_landscape(w, h) else "vertical"
    if pane_count >= TILED_THRESHOLD:
        return LayoutChoice(orientation=orientation, preset="tiled")
    if orientation == "horizontal":
        return LayoutChoice(
            orientation="horizontal",
            preset=LANDSCAPE_PRESET,
            options={"main-pane-width": MAIN_PANE_FRACTION},
        )
    return LayoutChoice(orientation="vertical", preset=PORTRAIT_PRESET)


def apply_adaptive(window_target: str) -> LayoutChoice | None:
    """Read window size + pane count from tmux, apply the matching preset."""
    if not window_target:
        return None
    if tmux.window_zoomed(window_target):
        # The human zoomed in on a member: a re-tile would both unzoom and
        # rearrange under them. Skip; the next unzoomed apply catches up.
        return None
    size = tmux.window_size(window_target)
    pane_count = len(tmux.list_panes(window_target))
    choice = pick(size, pane_count)
    if choice is None:
        return None
    for key, value in choice.options.items():
        tmux.set_window_option(window_target, key, value)
    tmux.select_layout(window_target, choice.preset)
    return choice


def split_horizontal(window_target: str, pane_count_after: int) -> bool:
    """Pick pre-spawn tmux split direction to match the final adaptive layout.

    Keeps the visible spawn geometry consistent with the post-spawn rebalance
    so portrait windows don't show a squeezed left-right split while the new
    CLI boots. Falls back to ``True`` (horizontal / ``-h``) when window size
    is unknown, matching the legacy default.
    """
    if not window_target:
        return True
    choice = pick(tmux.window_size(window_target), pane_count_after)
    if choice is None:
        return True
    return choice.orientation == "horizontal"
