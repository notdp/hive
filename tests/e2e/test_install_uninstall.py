"""Install/uninstall only disposable binary copies and isolated plugin state."""

import json
from pathlib import Path
import subprocess

import pytest

from ._helpers import ROOT, hive_binary_argv


def script(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
    path.chmod(0o755)


@pytest.fixture
def installation(tmp_path):
    # Build the environment from scratch: no host team, daemon, installer
    # override, plugin config, or real agent CLI is reachable through it.
    stub_bin = tmp_path / "stubs"
    env = {"PATH": f"{stub_bin}:/usr/bin:/bin"}
    for key in (
        "HOME", "HIVE_HOME", "CLAUDE_HOME", "CLAUDE_CONFIG_DIR",
        "CODEX_HOME", "GROK_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
        "TMPDIR", "TMUX_TMPDIR",
    ):
        directory = tmp_path / key
        directory.mkdir()
        env[key] = str(directory)
    env["HIVE_TEST_BINARY"] = hive_binary_argv()[0]
    env["CALL_LOG"] = str(tmp_path / "calls")
    env["STUB_BIN"] = str(Path(env["HOME"]) / ".cargo/bin")
    for cli in ("claude", "codex"):
        script(stub_bin / cli, f'''
printf '%s\\n' "{cli} $*" >> "$CALL_LOG"
if [ "${{CALL_PLUGIN_SOURCE:-}}" = 1 ] && [ "{cli} $*" = 'claude plugin install hive@hive --yes' ]; then
    cd "$HIVE_HOME"
    hive plugin sync >/dev/null
fi
if [ "${{FAIL_PLUGIN:-}}" = 1 ] && [ "{cli} $*" = 'claude plugin install hive@hive --yes' ]; then
    echo 'registration denied' >&2
    exit 1
fi
''')
    installer = tmp_path / "installer.sh"
    script(installer, '''
mkdir -p "$STUB_BIN" "$XDG_CONFIG_HOME/hive"
cp "$HIVE_TEST_BINARY" "$STUB_BIN/hive"
echo '{}' > "$XDG_CONFIG_HOME/hive/hive-receipt.json"
''')
    env["HIVE_INSTALLER_URL"] = installer.as_uri()
    return env, installer


def install(env):
    return subprocess.run(
        ["/bin/sh", str(ROOT / "install.sh")], env=env, cwd=env["HOME"],
        text=True, capture_output=True, timeout=30,
    )


def calls(env):
    path = Path(env["CALL_LOG"])
    return path.read_text().splitlines() if path.exists() else []


def assert_setup(env):
    rows = calls(env)
    assert len(rows) == 5, rows
    assert rows[0].startswith("claude plugin marketplace add ")
    assert rows[1] == "claude plugin install hive@hive --yes"
    assert rows[2] == "claude plugin update hive@hive --yes"
    assert rows[3].startswith("codex plugin marketplace add ")
    assert rows[4] == "codex plugin add hive@hive"


@pytest.mark.parametrize("mode", ["default", "cargo", "hive", "dist", "unmanaged", "unmanaged-cargo", "unmanaged-default"])
def test_install_resolves_the_selected_directory(installation, tmp_path, mode):
    env, _ = installation
    cargo = tmp_path / "cargo root"
    override = tmp_path / "custom root"
    if mode == "cargo":
        env["CARGO_HOME"] = str(cargo)
        env["STUB_BIN"] = str(cargo / "bin")
    elif mode in ("hive", "dist"):
        env["CARGO_HOME"] = str(cargo)
        env["HIVE_UNMANAGED_INSTALL"] = str(tmp_path / "unused")
        env["CARGO_DIST_FORCE_INSTALL_DIR"] = str(override)
        if mode == "hive":
            env["HIVE_INSTALL_DIR"] = str(tmp_path / "preferred")
            override = Path(env["HIVE_INSTALL_DIR"])
        env["STUB_BIN"] = str(override / "bin")
    elif mode.startswith("unmanaged"):
        if mode == "unmanaged-cargo":
            env["CARGO_HOME"] = str(cargo)
            override = cargo
        elif mode == "unmanaged-default":
            override = Path(env["HOME"]) / ".cargo"
        env["HIVE_UNMANAGED_INSTALL"] = str(override)
        env["STUB_BIN"] = str(override if mode == "unmanaged" else override / "bin")
        if mode != "unmanaged":
            # An old executable at the flat path must not win over /bin.
            script(override / "hive", 'echo wrong-binary >> "$CALL_LOG"; exit 9')
    result = install(env)
    assert result.returncode == 0, result.stdout + result.stderr
    assert (Path(env["STUB_BIN"]) / "hive").is_file()
    assert_setup(env)
    assert not list(Path(env["TMPDIR"]).iterdir())


def test_install_propagates_setup_failure_and_finishes_other_steps(installation):
    env, _ = installation
    env["FAIL_PLUGIN"] = "1"
    result = install(env)
    assert result.returncode == 1, result.stdout + result.stderr
    assert_setup(env)


def test_install_exposes_the_new_binary_to_plugin_source_commands(installation, tmp_path):
    env, _ = installation
    env["CALL_PLUGIN_SOURCE"] = "1"
    # A relative install override must still work if the CLI changes cwd
    # before evaluating the plugin's command source.
    env["HIVE_INSTALL_DIR"] = "relative-cargo"
    env["STUB_BIN"] = str(Path(env["HOME"]) / "relative-cargo/bin")
    script(tmp_path / "stubs/hive", "exit 9")
    result = install(env)
    assert result.returncode == 0, result.stdout + result.stderr
    assert_setup(env)


@pytest.mark.parametrize("failure", ["download", "installer", "missing-binary"])
def test_install_does_not_fall_back_to_an_old_binary(installation, tmp_path, failure):
    env, installer = installation
    script(Path(env["HOME"]) / ".cargo/bin/hive", 'echo old-binary >> "$CALL_LOG"')
    if failure == "download":
        env["HIVE_INSTALLER_URL"] = (tmp_path / "absent").as_uri()
    elif failure == "installer":
        script(installer, "exit 17")
    else:
        env["HIVE_INSTALL_DIR"] = str(tmp_path / "new-install")
        script(installer, "exit 0")
    result = install(env)
    assert result.returncode != 0, result.stdout + result.stderr
    assert calls(env) == []
    assert not list(Path(env["TMPDIR"]).iterdir())


@pytest.mark.parametrize("purge", [False, True])
def test_uninstall_removes_only_the_installed_copy(installation, purge):
    env, _ = installation
    result = install(env)
    assert result.returncode == 0, result.stdout + result.stderr
    binary = Path(env["STUB_BIN"]) / "hive"
    settings = Path(env["CODEX_HOME"]) / "config.toml"
    settings.write_text("model = 'kept'\n")
    result = subprocess.run(
        [str(binary), "uninstall", *(["--purge"] if purge else [])],
        env=env, cwd=env["HOME"], text=True, capture_output=True, timeout=30,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert not binary.exists()
    assert not (Path(env["XDG_CONFIG_HOME"]) / "hive/hive-receipt.json").exists()
    assert Path(env["HIVE_HOME"]).exists() is not purge
    assert settings.read_text() == "model = 'kept'\n"
    assert calls(env)[5:] == [
        "claude plugin uninstall hive@hive --scope user",
        "claude plugin marketplace remove hive --scope user",
        "codex plugin remove hive@hive",
        "codex plugin marketplace remove hive",
    ]


def test_uninstall_refuses_teams_without_running_plugin_commands(installation):
    env, _ = installation
    assert install(env).returncode == 0
    team_dir = Path(env["HIVE_HOME"]) / "teams/probe"
    team_dir.mkdir(parents=True)
    # Even a corrupt registry entry is a registered team, not an empty home.
    (team_dir / "team.json").write_text(json.dumps({"invalid": True}))
    binary = Path(env["STUB_BIN"]) / "hive"
    result = subprocess.run([str(binary), "uninstall", "--purge"], env=env,
                            cwd=env["HOME"], text=True, capture_output=True)
    assert result.returncode == 1
    assert "probe" in result.stderr and "--down" in result.stderr
    assert binary.exists() and team_dir.exists()
    assert_setup(env)
