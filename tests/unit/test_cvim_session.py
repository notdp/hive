import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SESSION_HELPER = ROOT / "src" / "hive" / "core_assets" / "cvim" / "bin" / "cvim-session"


def _run_session_helper(*, cwd: str, pane_id: str = "") -> str:
    args = [sys.executable, str(SESSION_HELPER), cwd]
    if pane_id:
        args.append(pane_id)
    result = subprocess.run(
        args,
        check=True,
        capture_output=True,
        text=True,
        env=os.environ.copy(),
    )
    return result.stdout.strip()


def test_session_helper_returns_empty_without_adapter(tmp_path):
    result = _run_session_helper(cwd="/repo", pane_id="%9")
    assert result == ""
