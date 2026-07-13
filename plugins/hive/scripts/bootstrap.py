#!/usr/bin/env python3
"""Bootstrap the hive CLI and Claude-side marketplace auto-update.

Ships inside the published hive plugin (stdlib only -- its job includes
installing the CLI, so it cannot depend on it). Two phases:

1. CLI check (never skipped): the active ``hive`` on PATH must exist and
   meet MIN_CLI_VERSION. Missing -> pipx install; old -> pipx force-reinstall,
   but only when the active binary provably belongs to pipx's hive venv.
   Every install is followed by a fresh probe; an unconverged system exits
   nonzero and phase 2 never runs.
2. Claude settings (gated by the updater switches): write the canonical
   ``extraKnownMarketplaces.hive`` entry with autoUpdate enabled. Any foreign
   or malformed shape fails closed with zero mutation.

Exit 0 means converged (including a legitimate settings skip); nonzero means
human action is needed and stderr carries a single-line remediation.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_URL = "git+https://github.com/notdp/hive"
MARKETPLACE_SOURCE = {"source": "github", "repo": "notdp/hive"}
MIN_CLI_VERSION = (0, 10, 0)
_VERSION_RE = re.compile(r"^hive, version (\d+)\.(\d+)\.(\d+)$")
_SUBPROCESS_TIMEOUT = 300


class BootstrapError(Exception):
    """Single-line remediation message; printed to stderr, exit nonzero."""


def _run(argv: list[str], runner=subprocess.run):
    try:
        out = runner(argv, capture_output=True, text=True, timeout=_SUBPROCESS_TIMEOUT)
    except subprocess.TimeoutExpired:
        raise BootstrapError(f"`{' '.join(argv)}` timed out after {_SUBPROCESS_TIMEOUT}s")
    return out


def _check(argv: list[str], runner=subprocess.run) -> None:
    out = _run(argv, runner=runner)
    if out.returncode != 0:
        detail = (out.stderr or out.stdout).strip().splitlines()
        raise BootstrapError(
            f"`{' '.join(argv)}` failed ({out.returncode}): {detail[-1][:200] if detail else 'no output'}")


def probe_version(which=shutil.which, runner=subprocess.run) -> tuple[int, int, int] | None:
    """Version of the active ``hive``, ``None`` when absent from PATH.

    The output contract is Click's exact shape ``hive, version X.Y.Z``;
    anything else is unparseable and fails loudly rather than guessing.
    """
    exe = which("hive")
    if not exe:
        return None
    out = _run([exe, "--version"], runner=runner)
    if out.returncode != 0:
        raise BootstrapError(f"`hive --version` failed ({out.returncode})")
    text = out.stdout.strip()
    m = _VERSION_RE.match(text)
    if m is None or "\n" in text:
        raise BootstrapError(f"cannot parse `hive --version` output: {text[:120]!r}")
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)))


def _pipx_app_paths(runner=subprocess.run) -> list[str]:
    """Real paths of the apps pipx installed for its ``hive`` venv.

    Fails closed on malformed JSON or unknown shapes -- an unprovable
    ownership claim must never authorize a force-reinstall.
    """
    out = _run(["pipx", "list", "--json"], runner=runner)
    if out.returncode != 0:
        raise BootstrapError(f"`pipx list --json` failed ({out.returncode})")
    try:
        data = json.loads(out.stdout)
        package = data["venvs"]["hive"]["metadata"]["main_package"]
        raw_paths = package["app_paths"]
    except (ValueError, KeyError, TypeError):
        raise BootstrapError(
            "cannot determine pipx ownership of `hive` (pipx list --json is "
            "malformed or has no hive venv); reinstall manually: "
            f"pipx install --force {REPO_URL}")
    if not isinstance(raw_paths, list):
        # a dict iterates by keys and a str by chars -- both are unknown
        # schemas that must never authorize a force install
        raise BootstrapError(
            "unrecognized app_paths shape in pipx list --json; reinstall "
            f"manually: pipx install --force {REPO_URL}")
    paths: list[str] = []
    for entry in raw_paths:
        if isinstance(entry, str):
            paths.append(entry)
        elif isinstance(entry, dict) and isinstance(entry.get("__Path__"), str):
            paths.append(entry["__Path__"])
        else:
            raise BootstrapError(
                "unrecognized app_paths shape in pipx list --json; reinstall "
                f"manually: pipx install --force {REPO_URL}")
    return [os.path.realpath(p) for p in paths]


def ensure_cli(which=shutil.which, runner=subprocess.run) -> str:
    """Converge the active ``hive`` to MIN_CLI_VERSION. Returns a summary."""
    version = probe_version(which=which, runner=runner)

    if version is None:
        if not which("pipx"):
            raise BootstrapError(
                f"hive is not on PATH and pipx is unavailable; install with: "
                f"pipx install {REPO_URL}")
        _check(["pipx", "install", REPO_URL], runner=runner)
        action = f"installed via pipx install {REPO_URL}"
    elif version < MIN_CLI_VERSION:
        exe = which("hive")
        active = os.path.realpath(exe)
        if active not in _pipx_app_paths(runner=runner):
            raise BootstrapError(
                f"active hive ({exe}) is not the pipx-managed binary; refusing "
                f"to overwrite. Remove the shadowing entry or reinstall "
                f"manually: pipx install --force {REPO_URL}")
        _check(["pipx", "install", "--force", REPO_URL], runner=runner)
        action = f"upgraded via pipx install --force {REPO_URL}"
    else:
        return f"hive {'.'.join(map(str, version))} already meets minimum"

    # subprocess success is not convergence: re-probe the active binary
    after = probe_version(which=which, runner=runner)
    if after is None or after < MIN_CLI_VERSION:
        raise BootstrapError(
            f"hive still {'missing' if after is None else '.'.join(map(str, after))} "
            f"after install ({action}); PATH may prefer another entry")
    return f"{action}; active hive {'.'.join(map(str, after))}"


def settings_path(environ=os.environ) -> Path:
    root = environ.get("CLAUDE_CONFIG_DIR")
    return (Path(root) if root else Path.home() / ".claude") / "settings.json"


def ensure_settings(path: Path | None = None, environ=os.environ) -> str:
    """Write the canonical hive marketplace entry with autoUpdate enabled."""
    if environ.get("DISABLE_AUTOUPDATER") and not environ.get("FORCE_AUTOUPDATE_PLUGINS"):
        return (
            "skipped: DISABLE_AUTOUPDATER is set without FORCE_AUTOUPDATE_PLUGINS, "
            "so Claude will not auto-update any plugin. To receive hive updates "
            "automatically, also set FORCE_AUTOUPDATE_PLUGINS=1; until then run "
            "`claude plugin update hive@hive` manually")

    if path is None:
        path = settings_path(environ=environ)
    original: bytes | None = None
    data: dict = {}
    if path.exists():
        original = path.read_bytes()
        try:
            data = json.loads(original)
        except ValueError:
            raise BootstrapError(f"{path} is not valid JSON; fix it manually")
        if not isinstance(data, dict):
            raise BootstrapError(f"{path} top level is not an object")

    markets = data.setdefault("extraKnownMarketplaces", {})
    if not isinstance(markets, dict):
        raise BootstrapError(f"{path}: extraKnownMarketplaces is not an object")
    entry = markets.get("hive")
    if entry is not None:
        if not isinstance(entry, dict):
            raise BootstrapError(f"{path}: extraKnownMarketplaces.hive is not an object")
        if entry.get("source") != MARKETPLACE_SOURCE:
            raise BootstrapError(
                f"{path}: extraKnownMarketplaces.hive has a foreign source "
                f"({entry.get('source')!r}); refusing to touch it")
        if entry.get("autoUpdate") is True:
            return "settings already converged"
        entry["autoUpdate"] = True
    else:
        markets["hive"] = {"source": dict(MARKETPLACE_SOURCE), "autoUpdate": True}

    payload = (json.dumps(data, indent=2, ensure_ascii=False) + "\n").encode()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=".settings-")
    try:
        os.write(fd, payload)
        os.close(fd)
        if original is not None:
            os.chmod(tmp, stat.S_IMODE(path.stat().st_mode))
        os.replace(tmp, path)
    except OSError:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return "settings updated: extraKnownMarketplaces.hive autoUpdate enabled"


def main() -> int:
    try:
        cli_summary = ensure_cli()
    except BootstrapError as e:
        print(f"bootstrap: {e}", file=sys.stderr)
        return 1
    try:
        settings_summary = ensure_settings()
    except BootstrapError as e:
        print(f"bootstrap: {e}", file=sys.stderr)
        return 1
    print(f"bootstrap: {cli_summary}")
    print(f"bootstrap: {settings_summary}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
