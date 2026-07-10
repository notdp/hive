import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SESSION_HELPER = ROOT / "src" / "hive" / "core_assets" / "cvim" / "bin" / "cvim-session"


def _run_session_helper(*, cwd: str, tmux_tmpdir: Path, pane_id: str = "") -> str:
    args = [sys.executable, str(SESSION_HELPER), cwd]
    if pane_id:
        args.append(pane_id)
    # Hermetic tmux: the helper resolves pane ids against a live tmux server,
    # so placeholder ids like %9 can hit a developer's real pane and return a
    # real transcript. Point server discovery at an existing empty directory
    # (no socket → every pane id fails to resolve) and drop the inherited
    # client identity.
    env = os.environ.copy()
    env.pop("TMUX", None)
    env.pop("TMUX_PANE", None)
    env["TMUX_TMPDIR"] = str(tmux_tmpdir)
    result = subprocess.run(
        args,
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    return result.stdout.strip()


def test_session_helper_returns_empty_without_adapter(tmp_path):
    result = _run_session_helper(cwd="/repo", pane_id="%9", tmux_tmpdir=tmp_path)
    assert result == ""
