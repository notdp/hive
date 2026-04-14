import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SESSION_HELPER = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin" / "cvim-session"


def _run_session_helper(
    *,
    cwd: str,
    droid_args: str = "",
    pane_id: str = "",
    factory_home: Path | None = None,
) -> str:
    env = os.environ.copy()
    if factory_home is not None:
        env["FACTORY_HOME"] = str(factory_home)
    args = [
        sys.executable,
        str(SESSION_HELPER),
        cwd,
        droid_args,
    ]
    if pane_id:
        args.append(pane_id)
    result = subprocess.run(
        args,
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    return result.stdout.strip()


def test_session_helper_uses_resume_transcript(tmp_path, monkeypatch):
    factory_home = tmp_path / "factory"
    monkeypatch.setenv("FACTORY_HOME", str(factory_home))

    cwd = "/repo"
    session_dir = factory_home / "sessions" / "-repo"
    session_dir.mkdir(parents=True)
    resumed = session_dir / "12345678-1234-1234-1234-123456789abc.jsonl"
    resumed.write_text("")

    result = _run_session_helper(
        cwd=cwd,
        droid_args="droid --resume 12345678-1234-1234-1234-123456789abc",
        pane_id="%9",
        factory_home=factory_home,
    )

    assert result == str(resumed)


def test_session_helper_returns_empty_without_resume_or_adapter(tmp_path, monkeypatch):
    factory_home = tmp_path / "factory"
    monkeypatch.setenv("FACTORY_HOME", str(factory_home))

    result = _run_session_helper(
        cwd="/repo",
        droid_args="droid",
        pane_id="%9",
        factory_home=factory_home,
    )

    assert result == ""
