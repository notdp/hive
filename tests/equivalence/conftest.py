"""Differential harness: same command through the Python CLI and the Rust
binary, byte-compared after volatile-field normalization.

Runs only when HIVE_RS_BIN points at a built Rust `hive`; the whole suite
skips otherwise, so it is inert until the port is being validated.
"""
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
# prog_name pins what every real invocation shows: the installed entry point
# (pyproject [project.scripts] hive = "hive.cli:cli") runs with argv[0]
# "hive", while this harness's `python -c` would leak click's detected prog
# name "-c" into every Usage:/error line — a capture artifact of the harness,
# not CLI behavior.
CLI_CODE = "from hive.cli import cli; cli(prog_name='hive')"


def rs_binary() -> str | None:
    return os.environ.get("HIVE_RS_BIN") or None


@pytest.fixture(scope="session", autouse=True)
def _require_rs_binary():
    binary = rs_binary()
    if not binary:
        pytest.skip("set HIVE_RS_BIN to the built Rust hive to run equivalence tests")
    if not Path(binary).is_file():
        pytest.fail(f"HIVE_RS_BIN={binary} is not a file")


# Volatile spans that legitimately differ run to run (never behavior).
_NORMALIZERS = (
    (re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?"), "<TS>"),
    (re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"), "<UUID>"),
    (re.compile(r"\b\d{10}\.\d+\b"), "<EPOCH>"),
    (re.compile(r"/(?:private/)?(?:tmp|var/folders)/[\w./-]+"), "<TMP>"),
)


def normalize(text: str, home: Path) -> str:
    text = text.replace(str(home), "<HOME>")
    for pattern, replacement in _NORMALIZERS:
        text = pattern.sub(replacement, text)
    return text.strip()


class Side:
    """One CLI implementation under a private HIVE_HOME."""

    def __init__(self, argv: list[str], home: Path, extra_env: dict[str, str]):
        self.argv = argv
        self.home = home
        self.env = {
            **{k: v for k, v in os.environ.items() if k in ("PATH", "LANG", "TERM", "USER")},
            "HIVE_HOME": str(home / ".hive"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "CLAUDE_CONFIG_DIR": str(home / ".claude"),
            "HOME": str(home),
            "PYTHONPATH": str(ROOT / "src"),
            "PYTHONUNBUFFERED": "1",
            "NO_COLOR": "1",
            **extra_env,
        }
        for key in ("HIVE_TEAM", "HIVE_MEMBER", "CODEX_THREAD_ID", "CLAUDE_CODE_MESSAGING_SOCKET", "TMUX", "TMUX_PANE"):
            self.env.pop(key, None)

    def run(self, args: list[str]) -> tuple[int, str]:
        proc = subprocess.run(
            [*self.argv, *args],
            env=self.env,
            cwd=str(self.home),
            capture_output=True,
            text=True,
            timeout=30,
        )
        merged = (proc.stdout or "") + (proc.stderr or "")
        return proc.returncode, normalize(merged, self.home)

    def seed_team(self, team: str, members: list[dict[str, str]] | None = None) -> None:
        teams = self.home / ".hive" / "state" / "teams"
        teams.mkdir(parents=True, exist_ok=True)
        (teams / f"{team}.json").write_text(
            json.dumps(
                {
                    "team": team,
                    "workspace": str(self.home / "ws" / team),
                    # Registry timestamps are stringified epoch floats, and
                    # readers float() them — an ISO string crashes the CLI.
                    "createdAt": "1788027853.880667",
                    "display": "",
                    "members": members or [],
                },
                indent=2,
                sort_keys=True,
            )
            + "\n"
        )
        (self.home / "ws" / team).mkdir(parents=True, exist_ok=True)


@pytest.fixture
def sides(tmp_path):
    """(python_side, rust_side) with identical, isolated state trees."""
    py_home = tmp_path / "py"
    rs_home = tmp_path / "rs"
    for home in (py_home, rs_home):
        home.mkdir()
    return (
        Side([sys.executable, "-c", CLI_CODE], py_home, {}),
        Side([rs_binary()], rs_home, {}),
    )
