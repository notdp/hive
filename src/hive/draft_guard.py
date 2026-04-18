"""Protect user drafts when injecting Hive messages into TUI input boxes.

A Hive send that naively does `send-keys -l <msg>` + Enter concatenates
whatever the user was typing with our message. This module saves the
draft, clears the input box, lets the caller inject + submit, then
pastes the draft back via bracketed paste so multi-line content does
not trigger an accidental submit.

Profiles differ in prompt glyph, baseline cursor_x, and clear-keys cost:

- claude: `❯ ` with NO-BREAK SPACE (U+00A0) separator; cursor_x=2 in
  empty state; C-u × 30 drains the input box
- codex:  `› ` (U+203A + 0x20); cursor_x=2 in empty state; C-u × 30
- droid:  box-bordered `│ > ...│`; cursor_x is unreliable (=0 whether
  empty or typed); falls back to placeholder pattern match; C-u × 15
"""

from __future__ import annotations

import time
from dataclasses import dataclass

from . import tmux


_CODEX_PROMPT = "› "
_CLAUDE_PROMPT = "❯\xa0"
_DROID_PLACEHOLDER_HINTS = ('Try "', 'Suggest ', 'Ask ')


@dataclass(frozen=True)
class ProfileConfig:
    name: str
    baseline_cursor_x: int | None
    clear_repetitions: int


_PROFILES: dict[str, ProfileConfig] = {
    "claude": ProfileConfig("claude", baseline_cursor_x=2, clear_repetitions=30),
    "codex":  ProfileConfig("codex",  baseline_cursor_x=2, clear_repetitions=30),
    "droid":  ProfileConfig("droid",  baseline_cursor_x=None, clear_repetitions=15),
}


def supported_profile(profile_name: str) -> bool:
    return profile_name in _PROFILES


def suspected_draft(pane_id: str, profile_name: str) -> bool:
    """Gate: return True when the input box is non-empty.

    Implemented by parsing the current capture. `cursor_x` was tried as
    a cheap signal earlier but proved unreliable — the user can paste
    content and move the cursor back to column 2 (empty baseline),
    producing a false negative and silent draft pollution.

    Parsing costs one `capture-pane` plus a profile-specific scan —
    measured at a few ms, worth paying every inject.
    """
    if profile_name not in _PROFILES:
        return False
    parser = _PARSERS.get(profile_name)
    if parser is None:
        return False
    return bool(parser(_capture_lines(pane_id)))


def parse_draft(pane_id: str, profile_name: str) -> str:
    """Parse the draft content from the TUI input box.

    Returns '' if no draft or profile is unsupported. Does not catch
    tmux errors — callers decide what to do on failure.
    """
    parser = _PARSERS.get(profile_name)
    if parser is None:
        return ""
    return parser(_capture_lines(pane_id))


def clear_input(pane_id: str, profile_name: str) -> None:
    """Clear the TUI input box with a profile-specific C-u barrage."""
    config = _PROFILES.get(profile_name)
    reps = config.clear_repetitions if config else 20
    tmux.send_keys_batch(pane_id, *["C-u"] * reps)


def wait_input_empty(
    pane_id: str,
    profile_name: str,
    *,
    timeout: float = 1.5,
    interval: float = 0.05,
) -> bool:
    """Poll until suspected_draft returns False. Return True on success."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not suspected_draft(pane_id, profile_name):
            return True
        time.sleep(interval)
    return False


def _capture_lines(pane_id: str) -> list[str]:
    height = tmux.display_value(pane_id, "#{pane_height}") or "80"
    try:
        lines_arg = max(int(height), 30)
    except ValueError:
        lines_arg = 80
    return tmux.capture_pane(pane_id, lines=lines_arg).splitlines()


def _droid_has_draft(lines: list[str]) -> bool:
    top, bot = _droid_box_bounds(lines)
    if top is None or bot is None or bot - top < 2:
        return False
    rows = lines[top + 1 : bot]
    if len(rows) != 1:
        return True
    row = rows[0]
    if not (row.startswith("│") and row.endswith("│")):
        return True
    inner = row[1:-1].strip()
    if not inner:
        return False
    if inner.startswith("> "):
        payload = inner[2:]
        for hint in _DROID_PLACEHOLDER_HINTS:
            if payload.startswith(hint):
                return False
        return True
    return True


def _droid_box_bounds(lines: list[str]) -> tuple[int | None, int | None]:
    top: int | None = None
    bot: int | None = None
    for i in range(len(lines) - 1, -1, -1):
        line = lines[i]
        if line.startswith("╰"):
            bot = i
        elif line.startswith("╭") and bot is not None:
            top = i
            break
    return top, bot


def _parse_claude(lines: list[str]) -> str:
    seps = [i for i, l in enumerate(lines) if l.startswith("─") and len(l) > 20]
    if len(seps) < 2:
        return ""
    top = seps[-2] + 1
    bot = seps[-1]
    block = lines[top:bot]
    return _strip_lines(block, first_prefix=_CLAUDE_PROMPT, cont_prefix="  ")


def _parse_codex(lines: list[str]) -> str:
    # Locate the last draft line (excluding status + trailing empty rows).
    i = len(lines) - 1
    while i >= 0 and lines[i].strip() == "":
        i -= 1
    while i >= 0 and lines[i].strip() != "":
        i -= 1
    while i >= 0 and lines[i].strip() == "":
        i -= 1
    end = i
    if end < 0:
        return ""
    # Walk upward for the `›` prompt row that opens the draft block.
    start = None
    for j in range(end, -1, -1):
        if lines[j].startswith(_CODEX_PROMPT):
            start = j
            break
    if start is None:
        return ""
    return _strip_lines(lines[start : end + 1], first_prefix=_CODEX_PROMPT, cont_prefix="  ")


def _parse_droid(lines: list[str]) -> str:
    top, bot = _droid_box_bounds(lines)
    if top is None or bot is None:
        return ""
    rows = lines[top + 1 : bot]
    stripped: list[str] = []
    for idx, row in enumerate(rows):
        if not (row.startswith("│") and row.endswith("│")):
            continue
        inner = row[1:-1]
        if idx == 0:
            if inner.startswith(" > "):
                text = inner[3:].rstrip()
            else:
                text = inner.strip()
        else:
            if inner.startswith("   "):
                text = inner[3:].rstrip()
            else:
                text = inner.strip()
        stripped.append(text)
    if len(stripped) == 1:
        hint = stripped[0]
        for placeholder in _DROID_PLACEHOLDER_HINTS:
            if hint.startswith(placeholder):
                return ""
    return "\n".join(stripped)


def _strip_lines(lines: list[str], *, first_prefix: str, cont_prefix: str) -> str:
    out: list[str] = []
    for idx, line in enumerate(lines):
        if idx == 0 and line.startswith(first_prefix):
            rest = line[len(first_prefix):]
            # Some TUIs (Codex) render `›  <text>` when the user pasted with
            # a leading space; drop one extra leading space to avoid a
            # phantom space in the restored draft.
            if rest.startswith(" "):
                rest = rest[1:]
            out.append(rest)
        elif line.startswith(cont_prefix):
            out.append(line[len(cont_prefix):])
        else:
            out.append(line)
    return "\n".join(out)


_PARSERS = {
    "claude": _parse_claude,
    "codex": _parse_codex,
    "droid": _parse_droid,
}
