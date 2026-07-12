"""Unit tests for the published plugin's bootstrap script.

Loaded via importlib (the script lives in the plugin distribution surface,
not the hive package). All PATH probes and subprocesses are injected fakes:
no network, no real pipx, no live homes.
"""
import importlib.util
import json
import os
import stat
import subprocess
from pathlib import Path

import pytest

pytestmark = pytest.mark.unit

REPO = Path(__file__).resolve().parents[2]
_SPEC = importlib.util.spec_from_file_location(
    "hive_bootstrap", REPO / "plugins" / "hive" / "scripts" / "bootstrap.py")
bootstrap = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(bootstrap)

GOOD = "hive, version 0.9.5\n"
OLD = "hive, version 0.9.4\n"
PIPX_BIN = "/fake/pipx/venvs/hive/bin/hive"


class R:
    def __init__(self, rc=0, out="", err=""):
        self.returncode, self.stdout, self.stderr = rc, out, err


def which_for(table):
    return lambda name: table.get(name)


def runner_for(table, calls=None):
    """Dispatch on argv prefix; list values are consumed in sequence."""
    calls = calls if calls is not None else []

    def run(argv, capture_output=True, text=True, timeout=None):
        assert timeout is not None
        calls.append(list(argv))
        for prefix, result in table:
            if argv[: len(prefix)] == prefix:
                if isinstance(result, list):
                    result = result.pop(0)
                if isinstance(result, Exception):
                    raise result
                return result
        raise AssertionError(f"unexpected argv: {argv}")

    run.calls = calls
    return run


def pipx_json(app_path=PIPX_BIN):
    return json.dumps(
        {"venvs": {"hive": {"metadata": {"main_package": {
            "app_paths": [{"__Path__": app_path}]}}}}})


# --- version contract (VAL-1) ------------------------------------------------

def test_probe_version_parses_exact_click_shape():
    runner = runner_for([(["/x/hive", "--version"], R(out=GOOD))])
    assert bootstrap.probe_version(which_for({"hive": "/x/hive"}), runner) == (0, 9, 5)


def test_probe_version_missing_binary_is_none():
    assert bootstrap.probe_version(which_for({}), runner_for([])) is None


@pytest.mark.parametrize("out", [
    "0.9.5\n",                          # bare semver: not the real shape
    "hive, version 0.9\n",              # missing segment
    "hive, version 0.9.5.1\n",          # extra segment
    "hive, version abc\n",              # non-numeric
    "warning: x\nhive, version 0.9.5\n",  # extra stdout line
    "hive, version 0.9.5 (dev)\n",      # trailing garbage
])
def test_probe_version_rejects_non_contract_output(out):
    runner = runner_for([(["/x/hive", "--version"], R(out=out))])
    with pytest.raises(bootstrap.BootstrapError):
        bootstrap.probe_version(which_for({"hive": "/x/hive"}), runner)


def test_probe_version_nonzero_exit_fails():
    runner = runner_for([(["/x/hive", "--version"], R(rc=2))])
    with pytest.raises(bootstrap.BootstrapError):
        bootstrap.probe_version(which_for({"hive": "/x/hive"}), runner)


# --- CLI state machine (VAL-2) -----------------------------------------------

def test_missing_hive_missing_pipx_gives_remediation():
    with pytest.raises(bootstrap.BootstrapError, match=r"pipx install git\+"):
        bootstrap.ensure_cli(which_for({}), runner_for([]))


def test_missing_hive_installs_then_reprobes_converged():
    seen = {}

    def which(name):
        if name == "pipx":
            return "/x/pipx"
        return "/x/hive" if seen.get("installed") else None

    def mark_install(argv, **kw):
        seen["installed"] = True
        return R()

    runner = runner_for([
        (["pipx", "install", bootstrap.REPO_URL], mark_install(None) if False else R()),
        (["/x/hive", "--version"], R(out=GOOD)),
    ])
    # install must flip which() before the re-probe
    orig = runner

    def run(argv, **kw):
        out = orig(argv, **kw)
        if argv[:2] == ["pipx", "install"]:
            seen["installed"] = True
        return out

    run.calls = orig.calls
    summary = bootstrap.ensure_cli(which, run)
    assert "installed" in summary
    assert ["pipx", "install", bootstrap.REPO_URL] in run.calls


def test_missing_hive_install_that_does_not_converge_fails():
    which = which_for({"pipx": "/x/pipx"})  # hive stays missing after install
    runner = runner_for([(["pipx", "install", bootstrap.REPO_URL], R())])
    with pytest.raises(bootstrap.BootstrapError, match="after install"):
        bootstrap.ensure_cli(which, runner)


def test_current_hive_never_calls_pipx():
    runner = runner_for([(["/x/hive", "--version"], R(out=GOOD))])
    summary = bootstrap.ensure_cli(which_for({"hive": "/x/hive"}), runner)
    assert "already meets minimum" in summary
    assert all(c[0] != "pipx" for c in runner.calls)


def test_old_hive_owned_by_pipx_forces_reinstall_and_reprobes():
    runner = runner_for([
        ([PIPX_BIN, "--version"], [R(out=OLD), R(out=GOOD)]),
        (["pipx", "list", "--json"], R(out=pipx_json())),
        (["pipx", "install", "--force", bootstrap.REPO_URL], R()),
    ])
    summary = bootstrap.ensure_cli(which_for({"hive": PIPX_BIN, "pipx": "/x/pipx"}), runner)
    assert "--force" in summary and "0.9.5" in summary
    assert ["pipx", "install", "--force", bootstrap.REPO_URL] in runner.calls


def test_old_hive_shadowed_binary_fails_closed_zero_install():
    runner = runner_for([
        (["/other/hive", "--version"], R(out=OLD)),
        (["pipx", "list", "--json"], R(out=pipx_json())),  # owns PIPX_BIN, not /other/hive
    ])
    with pytest.raises(bootstrap.BootstrapError, match="refusing to overwrite"):
        bootstrap.ensure_cli(which_for({"hive": "/other/hive", "pipx": "/x/pipx"}), runner)
    assert all(c[:2] != ["pipx", "install"] for c in runner.calls)


@pytest.mark.parametrize("pipx_out", [
    "not json",
    json.dumps({"venvs": {}}),
    json.dumps({"venvs": {"hive": {"metadata": {}}}}),
    json.dumps({"venvs": {"hive": {"metadata": {"main_package": {"app_paths": [42]}}}}}),
    # dict iterates by keys: a key equal to the active realpath must not
    # authorize a force install (validator r1)
    json.dumps({"venvs": {"hive": {"metadata": {"main_package": {"app_paths": {PIPX_BIN: 42}}}}}}),
    json.dumps({"venvs": {"hive": {"metadata": {"main_package": {"app_paths": PIPX_BIN}}}}}),
])
def test_old_hive_unprovable_ownership_fails_closed(pipx_out):
    runner = runner_for([
        ([PIPX_BIN, "--version"], R(out=OLD)),
        (["pipx", "list", "--json"], R(out=pipx_out)),
    ])
    with pytest.raises(bootstrap.BootstrapError):
        bootstrap.ensure_cli(which_for({"hive": PIPX_BIN, "pipx": "/x/pipx"}), runner)
    assert all(c[:2] != ["pipx", "install"] for c in runner.calls)


def test_old_hive_reinstall_that_stays_old_fails():
    runner = runner_for([
        ([PIPX_BIN, "--version"], [R(out=OLD), R(out=OLD)]),
        (["pipx", "list", "--json"], R(out=pipx_json())),
        (["pipx", "install", "--force", bootstrap.REPO_URL], R()),
    ])
    with pytest.raises(bootstrap.BootstrapError, match="after install"):
        bootstrap.ensure_cli(which_for({"hive": PIPX_BIN, "pipx": "/x/pipx"}), runner)


def test_install_failure_surfaces_short_error():
    which = which_for({"pipx": "/x/pipx"})
    runner = runner_for([(["pipx", "install", bootstrap.REPO_URL], R(rc=1, err="boom\nlast line"))])
    with pytest.raises(bootstrap.BootstrapError, match="last line"):
        bootstrap.ensure_cli(which, runner)


def test_subprocess_timeout_becomes_bootstrap_error():
    runner = runner_for([
        (["/x/hive", "--version"], subprocess.TimeoutExpired(["hive"], 1)),
    ])
    with pytest.raises(bootstrap.BootstrapError, match="timed out"):
        bootstrap.probe_version(which_for({"hive": "/x/hive"}), runner)


# --- settings editor (VAL-3) -------------------------------------------------

CANONICAL = {"source": {"source": "github", "repo": "notdp/hive"}, "autoUpdate": True}


def _env(tmp_path, **extra):
    return {"CLAUDE_CONFIG_DIR": str(tmp_path), **extra}


def test_settings_created_when_missing(tmp_path):
    out = bootstrap.ensure_settings(environ=_env(tmp_path))
    data = json.loads((tmp_path / "settings.json").read_text())
    assert data["extraKnownMarketplaces"]["hive"] == CANONICAL
    assert "updated" in out


def test_settings_preserves_unrelated_keys_and_mode(tmp_path):
    p = tmp_path / "settings.json"
    p.write_text(json.dumps({"model": "opus", "env": {"A": "1"}}))
    p.chmod(0o640)
    bootstrap.ensure_settings(environ=_env(tmp_path))
    data = json.loads(p.read_text())
    assert data["model"] == "opus" and data["env"] == {"A": "1"}
    assert data["extraKnownMarketplaces"]["hive"] == CANONICAL
    assert stat.S_IMODE(p.stat().st_mode) == 0o640


def test_settings_already_converged_is_byte_identical(tmp_path):
    p = tmp_path / "settings.json"
    bootstrap.ensure_settings(environ=_env(tmp_path))
    before = p.read_bytes()
    out = bootstrap.ensure_settings(environ=_env(tmp_path))
    assert "already converged" in out
    assert p.read_bytes() == before


@pytest.mark.parametrize("content", [
    "not json",
    json.dumps(["top-level-array"]),
    json.dumps({"extraKnownMarketplaces": "not-an-object"}),
    json.dumps({"extraKnownMarketplaces": {"hive": "not-an-object"}}),
    json.dumps({"extraKnownMarketplaces": {"hive": {"source": {"source": "directory", "path": "/x"}}}}),
    json.dumps({"extraKnownMarketplaces": {"hive": {"source": {"source": "github"}}}}),
])
def test_settings_bad_shapes_fail_closed_zero_mutation(tmp_path, content):
    p = tmp_path / "settings.json"
    p.write_text(content)
    p.chmod(0o640)
    before = (p.read_bytes(), p.stat().st_mode, p.stat().st_mtime_ns)
    with pytest.raises(bootstrap.BootstrapError):
        bootstrap.ensure_settings(environ=_env(tmp_path))
    assert (p.read_bytes(), p.stat().st_mode, p.stat().st_mtime_ns) == before
    assert not list(tmp_path.glob(".settings-*"))  # no temp litter


def test_settings_root_prefers_claude_config_dir(tmp_path, monkeypatch):
    home = tmp_path / "home"
    cfg = tmp_path / "cfg"
    home.mkdir(), cfg.mkdir()
    monkeypatch.setenv("HOME", str(home))
    assert bootstrap.settings_path({"CLAUDE_CONFIG_DIR": str(cfg)}) == cfg / "settings.json"
    assert bootstrap.settings_path({}) == Path(os.path.expanduser("~")) / ".claude" / "settings.json"


def test_settings_skipped_by_global_lock_without_exemption(tmp_path):
    p = tmp_path / "settings.json"
    p.write_text("{}")
    before = p.read_bytes()
    out = bootstrap.ensure_settings(environ=_env(tmp_path, DISABLE_AUTOUPDATER="1"))
    assert out.startswith("skipped")
    assert "FORCE_AUTOUPDATE_PLUGINS" in out
    assert p.read_bytes() == before


def test_settings_written_when_both_switches_set(tmp_path):
    out = bootstrap.ensure_settings(
        environ=_env(tmp_path, DISABLE_AUTOUPDATER="1", FORCE_AUTOUPDATE_PLUGINS="1"))
    assert "updated" in out
    data = json.loads((tmp_path / "settings.json").read_text())
    assert data["extraKnownMarketplaces"]["hive"] == CANONICAL


# --- phase order (VAL-3) -----------------------------------------------------

def test_main_skips_settings_when_cli_phase_fails(tmp_path, monkeypatch, capsys):
    touched = []
    monkeypatch.setattr(bootstrap, "ensure_cli", lambda **kw: (_ for _ in ()).throw(
        bootstrap.BootstrapError("cli broken")))
    monkeypatch.setattr(bootstrap, "ensure_settings", lambda **kw: touched.append(True))
    assert bootstrap.main() == 1
    assert touched == []
    assert "cli broken" in capsys.readouterr().err


def test_main_runs_both_phases_in_order(monkeypatch, capsys):
    order = []
    monkeypatch.setattr(bootstrap, "ensure_cli", lambda **kw: order.append("cli") or "cli ok")
    monkeypatch.setattr(bootstrap, "ensure_settings", lambda **kw: order.append("settings") or "s ok")
    assert bootstrap.main() == 0
    assert order == ["cli", "settings"]
