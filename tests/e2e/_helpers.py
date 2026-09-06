import os
import re
import shlex
import subprocess
import time
import uuid
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def base_env(tmp_path: Path) -> dict[str, str]:
    # TMUX_TMPDIR puts every session — and the server-global key binding a
    # window build installs — on a private tmux server, never the
    # developer's.
    # CODEX_HOME must be private too: a hived's codex supervisor reaps the
    # pane→thread records of every pane its own tmux server cannot see,
    # and on the shared ~/.codex that is every live member's record.
    return {
        "HIVE_HOME": str(tmp_path / ".hive"),
        "XDG_CACHE_HOME": str(tmp_path / ".cache"),
        "TMUX_TMPDIR": str(tmp_path),
        "CODEX_HOME": str(tmp_path / ".codex"),
    }


def private_socket(env: dict[str, str]) -> str:
    """The socket of the private server `env` names via TMUX_TMPDIR. A
    harness client always passes it with `-S`: on a missing TMUX_TMPDIR
    (the workdir already removed) tmux silently falls through to the
    developer's default server, and a teardown `kill-server` sent that
    way kills the live sessions."""
    sock_dir = Path(env["TMUX_TMPDIR"]) / f"tmux-{os.getuid()}"
    # tmux creates this directory itself only when resolving the socket
    # from TMUX_TMPDIR; a `-S` client needs it there before a server can
    # bind (`hive`, which reads the env, lands on the same path).
    sock_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    return str(sock_dir / "default")


def tmux_argv(args: list[str], env: dict[str, str] | None) -> list[str]:
    if env and "TMUX_TMPDIR" in env:
        return ["tmux", "-S", private_socket(env), *args]
    return ["tmux", *args]


def run_tmux(args: list[str], *, env: dict[str, str] | None = None, timeout: int = 10) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        tmux_argv(args, env), env={**os.environ, **(env or {})}, text=True, capture_output=True, timeout=timeout, check=True
    )


def kill_private_server(env: dict[str, str]) -> None:
    """Teardown: kill the private server `env` names, by explicit socket."""
    subprocess.run(["tmux", "-S", private_socket(env), "kill-server"], capture_output=True, text=True)


def send_tmux_command(pane_id: str, text: str, *, env: dict[str, str] | None = None) -> None:
    run_tmux(["send-keys", "-t", pane_id, "-l", text], env=env)
    run_tmux(["send-keys", "-t", pane_id, "Enter"], env=env)


def wait_for(predicate, *, timeout: float = 10.0, interval: float = 0.05) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return
        time.sleep(interval)
    raise AssertionError("timed out waiting for condition")


def hive_binary_argv() -> list[str]:
    """How the suite invokes hive: HIVE_E2E_BIN when set, else this
    checkout's debug build (cargo build first)."""
    override = os.environ.get("HIVE_E2E_BIN")
    if override:
        return [override]
    debug = ROOT / "target" / "debug" / "hive"
    if debug.is_file():
        return [str(debug)]
    raise RuntimeError("no hive binary: run `cargo build` or set HIVE_E2E_BIN")


def hive_shell_command(args: list[str], *, env: dict[str, str], cwd: Path, stdout_path: Path) -> str:
    env_prefix = " ".join(f"{key}={shlex.quote(value)}" for key, value in env.items())
    cmd = " ".join([
        env_prefix,
        *(shlex.quote(part) for part in hive_binary_argv()),
        *(shlex.quote(arg) for arg in args),
    ])
    return f"cd {shlex.quote(str(cwd))} && {cmd} > {shlex.quote(str(stdout_path))} 2>&1"


def run_hive_in_tmux_pane(
    pane_id: str,
    args: list[str],
    *,
    env: dict[str, str],
    cwd: Path,
    timeout: float = 60.0,
    capture_lines: int = 200,
) -> subprocess.CompletedProcess[str]:
    # The pane runs the user's interactive shell, and a prompt with
    # completion plugins can take over ten seconds just to consume the
    # ~300-character command line before hive even starts.
    marker = f"__HIVE_DONE_{uuid.uuid4().hex}__"
    marker_pattern = re.compile(rf"^{re.escape(marker)}:(\d+)$")
    output_path = cwd / f".hive-tmux-{uuid.uuid4().hex}.out"
    send_tmux_command(pane_id, f"{hive_shell_command(args, env=env, cwd=cwd, stdout_path=output_path)}; printf '\\n{marker}:%s\\n' $?", env=env)

    def capture() -> str:
        return run_tmux(["capture-pane", "-t", pane_id, "-p", "-S", f"-{capture_lines}"], env=env).stdout

    def status_code() -> int | None:
        for line in reversed(capture().splitlines()):
            match = marker_pattern.fullmatch(line.strip())
            if match:
                return int(match.group(1))
        return None

    try:
        wait_for(lambda: status_code() is not None, timeout=timeout)
    except AssertionError as exc:
        raise AssertionError(f"timed out waiting for tmux command completion:\n{capture()}") from exc
    returncode = status_code()
    assert returncode is not None
    stdout = output_path.read_text() if output_path.exists() else ""
    output_path.unlink(missing_ok=True)
    return subprocess.CompletedProcess([*hive_binary_argv(), *args], returncode, stdout, "")
