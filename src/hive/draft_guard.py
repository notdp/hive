"""Protect user drafts when injecting text into TUI input boxes.

Normal Hive delivery never touches the composer — claude goes over its
per-pane MCP channel and codex over its per-pane daemon RPC. This module
only serves the retained keystroke surfaces (`hive inject` debugging and
the `/compact` TUI control): a naive `send-keys -l <msg>` + Enter would
concatenate whatever the user was typing with the injected text, so this
saves the draft, clears the input box, lets the caller inject + submit,
then pastes the draft back via bracketed paste so multi-line content does
not trigger an accidental submit.

Profiles differ in prompt glyph, baseline cursor_x, and clear-keys cost:

- claude: `❯ ` with NO-BREAK SPACE (U+00A0) separator; cursor_x=2 in
  empty state; C-u × 30 drains the input box
- codex:  `› ` (U+203A + 0x20); cursor_x=2 in empty state; C-u × 30
"""

from __future__ import annotations

import time
from dataclasses import dataclass

from . import tmux


_CODEX_PROMPT = "› "
_CLAUDE_PROMPT = "❯\xa0"


@dataclass(frozen=True)
class ProfileConfig:
    name: str
    baseline_cursor_x: int | None
    clear_repetitions: int


@dataclass(frozen=True)
class _StyledChar:
    value: str
    dim: bool = False
    reverse: bool = False


_PROFILES: dict[str, ProfileConfig] = {
    "claude": ProfileConfig("claude", baseline_cursor_x=2, clear_repetitions=30),
    "codex":  ProfileConfig("codex",  baseline_cursor_x=2, clear_repetitions=30),
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
    return bool(parser(_capture_lines(pane_id, profile_name)))


def parse_draft(pane_id: str, profile_name: str) -> str:
    """Parse the draft content from the TUI input box.

    Returns '' if no draft or profile is unsupported. Does not catch
    tmux errors — callers decide what to do on failure.
    """
    parser = _PARSERS.get(profile_name)
    if parser is None:
        return ""
    return parser(_capture_lines(pane_id, profile_name))


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


def _capture_lines(pane_id: str, profile_name: str) -> list[str]:
    height = tmux.display_value(pane_id, "#{pane_height}") or "80"
    try:
        lines_arg = max(int(height), 30)
    except ValueError:
        lines_arg = 80
    if profile_name in {"claude", "codex"}:
        return tmux.capture_pane(pane_id, lines=lines_arg, preserve_styles=True).splitlines()
    return tmux.capture_pane(pane_id, lines=lines_arg).splitlines()


def _parse_claude(lines: list[str]) -> str:
    seps = [i for i, l in enumerate(lines) if _visible_text(l).startswith("─") and len(_visible_text(l)) > 20]
    if len(seps) < 2:
        return ""
    top = seps[-2] + 1
    bot = seps[-1]
    block = lines[top:bot]
    return _strip_styled_lines(block, first_prefix=_CLAUDE_PROMPT)


def _parse_codex(lines: list[str]) -> str:
    # Locate the last draft line (excluding status + trailing empty rows).
    i = len(lines) - 1
    while i >= 0 and _visible_text(lines[i]).strip() == "":
        i -= 1
    while i >= 0 and _visible_text(lines[i]).strip() != "":
        i -= 1
    while i >= 0 and _visible_text(lines[i]).strip() == "":
        i -= 1
    end = i
    if end < 0:
        return ""
    # Walk upward for the `›` prompt row that opens the draft block.
    start = None
    for j in range(end, -1, -1):
        if _visible_text(lines[j]).startswith(_CODEX_PROMPT):
            start = j
            break
    if start is None:
        return ""
    return _strip_styled_lines(lines[start : end + 1], first_prefix=_CODEX_PROMPT)


def _strip_styled_lines(lines: list[str], *, first_prefix: str) -> str:
    out: list[str] = []
    for idx, line in enumerate(lines):
        cells = _styled_chars(line)
        if idx == 0:
            cells = _drop_visible_prefix(cells, first_prefix)
            cells = _drop_autocomplete_hint_cells(cells)
            # Match the old plain-text parser: if the prompt rendering leaves
            # one extra leading space before draft text, drop that boundary
            # space only on the first line.
            if cells and cells[0].value == " ":
                cells = cells[1:]
        else:
            cells = _drop_visible_prefix(cells, "  ")
        out.append("".join(cell.value for cell in cells))
    return "\n".join(out)


def _drop_visible_prefix(cells: list[_StyledChar], prefix: str) -> list[_StyledChar]:
    if "".join(cell.value for cell in cells[:len(prefix)]) == prefix:
        return cells[len(prefix):]
    return cells


def _drop_visible_suffix(cells: list[_StyledChar], suffix: str) -> list[_StyledChar]:
    if suffix and "".join(cell.value for cell in cells[-len(suffix):]) == suffix:
        return cells[:-len(suffix)]
    return cells


def _rstrip_cells(cells: list[_StyledChar]) -> list[_StyledChar]:
    end = len(cells)
    while end > 0 and cells[end - 1].value == " ":
        end -= 1
    return cells[:end]


def _drop_autocomplete_hint_cells(cells: list[_StyledChar]) -> list[_StyledChar]:
    first_dim = next((idx for idx, cell in enumerate(cells) if cell.dim), None)
    if first_dim is None:
        return cells
    start = first_dim
    while start > 0 and cells[start - 1].reverse:
        start -= 1
    return cells[:start]


def _visible_text(line: str) -> str:
    return "".join(cell.value for cell in _styled_chars(line))


def _styled_chars(line: str) -> list[_StyledChar]:
    cells: list[_StyledChar] = []
    dim = False
    reverse = False
    i = 0
    while i < len(line):
        if line[i] == "\x1b" and i + 1 < len(line) and line[i + 1] == "[":
            end = line.find("m", i + 2)
            if end != -1:
                codes = line[i + 2:end]
                params = [0] if codes == "" else _parse_sgr_codes(codes)
                dim, reverse = _apply_sgr(params, dim=dim, reverse=reverse)
                i = end + 1
                continue
        cells.append(_StyledChar(line[i], dim=dim, reverse=reverse))
        i += 1
    return cells


def _parse_sgr_codes(raw: str) -> list[int]:
    codes: list[int] = []
    for part in raw.split(";"):
        if not part:
            codes.append(0)
            continue
        try:
            codes.append(int(part))
        except ValueError:
            continue
    return codes


def _apply_sgr(params: list[int], *, dim: bool, reverse: bool) -> tuple[bool, bool]:
    i = 0
    while i < len(params):
        code = params[i]
        if code == 0:
            dim = False
            reverse = False
        elif code == 2:
            dim = True
        elif code == 7:
            reverse = True
        elif code == 22:
            dim = False
        elif code == 27:
            reverse = False
        elif code in {38, 48}:
            if i + 1 < len(params):
                mode = params[i + 1]
                if mode == 2:
                    i += 4
                elif mode == 5:
                    i += 2
        i += 1
    return dim, reverse


_PARSERS = {
    "claude": _parse_claude,
    "codex": _parse_codex,
}
