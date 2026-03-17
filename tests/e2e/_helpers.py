import os
import shlex
import subprocess
import sys
import textwrap
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CLI_CODE = "from hive.cli import cli; cli()"


def base_env(tmp_path: Path, fake_droid: Path) -> dict[str, str]:
    pythonpath = str(ROOT / "src")
    if os.environ.get("PYTHONPATH"):
        pythonpath = f"{pythonpath}{os.pathsep}{os.environ['PYTHONPATH']}"
    return {
        "HIVE_HOME": str(tmp_path / ".hive"),
        "DROID_PATH": str(fake_droid),
        "PYTHONPATH": pythonpath,
        "PYTHONUNBUFFERED": "1",
    }


def run_hive(args: list[str], *, env: dict[str, str], cwd: Path, timeout: int = 20) -> subprocess.CompletedProcess[str]:
    merged_env = os.environ.copy()
    merged_env.update(env)
    merged_env.pop("TMUX", None)
    merged_env.pop("TMUX_PANE", None)
    return subprocess.run(
        [sys.executable, "-c", CLI_CODE, *args],
        cwd=str(cwd),
        env=merged_env,
        text=True,
        capture_output=True,
        timeout=timeout,
    )


def run_tmux(args: list[str], *, timeout: int = 10) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["tmux", *args], text=True, capture_output=True, timeout=timeout, check=True)


def send_tmux_command(pane_id: str, text: str) -> None:
    run_tmux(["send-keys", "-t", pane_id, "-l", text])
    run_tmux(["send-keys", "-t", pane_id, "Enter"])


def wait_for(predicate, *, timeout: float = 5.0, interval: float = 0.05) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return
        time.sleep(interval)
    raise AssertionError("timed out waiting for condition")


def wait_for_file(path: Path) -> str:
    wait_for(lambda: path.exists() and path.read_text().strip() != "")
    return path.read_text()


def write_fake_droid(tmp_path: Path) -> Path:
    path = tmp_path / "fake-droid.py"
    path.write_text(
        textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import sys

            print("fake droid ready - type 'for help'")
            sys.stdout.flush()

            for line in sys.stdin:
                text = line.rstrip("\\n")
                print(f"RECV:{text}")
                sys.stdout.flush()
            """
        )
    )
    path.chmod(0o755)
    return path


def hive_shell_command(args: list[str], *, env: dict[str, str], cwd: Path, stdout_path: Path) -> str:
    env_prefix = " ".join(f"{key}={shlex.quote(value)}" for key, value in env.items())
    cmd = " ".join([
        env_prefix,
        shlex.quote(sys.executable),
        "-c",
        shlex.quote(CLI_CODE),
        *(shlex.quote(arg) for arg in args),
    ])
    return f"cd {shlex.quote(str(cwd))} && {cmd} > {shlex.quote(str(stdout_path))} 2>&1"
