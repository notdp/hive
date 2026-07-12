"""Boundary invariant: only src/hive/tmux.py may build tmux argv.

Everything else routes through the facade -- including Python source
embedded in raw strings (notify's pane-attention script), which this scan
covers because it matches file text, not the AST.
"""
import re
from pathlib import Path

import pytest

pytestmark = pytest.mark.unit

SRC = Path(__file__).resolve().parents[2] / "src" / "hive"

# an argv whose first element is tmux: ["tmux", ... / ('tmux', ...
_TMUX_ARGV = re.compile(r'''[\[(]\s*['"]tmux['"]\s*,''')


def _scan(text: str) -> list[int]:
    """Line numbers of tmux-argv matches in full text (multiline included)."""
    return [text.count("\n", 0, m.start()) + 1 for m in _TMUX_ARGV.finditer(text)]


def _offenders() -> list[str]:
    found = []
    for path in sorted(SRC.rglob("*.py")):
        if path.name == "tmux.py":
            continue
        for lineno in _scan(path.read_text()):
            found.append(f"{path.relative_to(SRC)}:{lineno}")
    return found


def test_only_the_facade_builds_tmux_argv():
    assert _offenders() == []


@pytest.mark.parametrize("snippet", [
    'subprocess.run(["tmux", "display-message"])',
    "cmd = ['tmux', 'display-popup']",
    'result = _run(  [ "tmux" , "ls"])',
    # multiline argv is the common formatter shape; must hit the same
    # scanner path as the tree walk (validator r1)
    'result = subprocess.run(\n    [\n        "tmux",\n        "ls",\n    ]\n)',
])
def test_scan_rejects_argv_shapes(snippet):
    assert _scan(snippet), snippet


@pytest.mark.parametrize("snippet", [
    'result["tmux"] = {"session": name}',   # dict assignment, not argv
    '{"tmux": tmux_status, "teams": teams}',  # dict literal key... ',' follows value not key
    "# the tmux server may restart",        # prose
    'payload.get("tmux") == "unknown"',     # lookup
])
def test_scan_ignores_non_argv_uses(snippet):
    assert not _scan(snippet)


def test_scan_reports_correct_line_number():
    text = 'x = 1\ny = subprocess.run(\n    ["tmux",\n     "ls"])\n'
    assert _scan(text) == [3]
