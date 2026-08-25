import json

import pytest

from hive import plugin_manager
from hive.cli import cli


def test_plugin_list_does_not_offer_retired_plugins(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)
    listed = runner.invoke(cli, ["plugin", "list", "--json"])
    assert listed.exit_code == 0
    names = {item["name"] for item in json.loads(listed.output)}
    assert "cvim" not in names
    assert "fork" not in names
    assert "code-review" not in names
    assert "notify" in names


@pytest.mark.parametrize("retired", ["cvim", "fork", "code-review"])
def test_plugin_enable_rejects_retired_plugins(runner, configure_hive_home, retired):
    configure_hive_home(tmux_inside=False)
    result = runner.invoke(cli, ["plugin", "enable", retired])
    assert result.exit_code != 0
    assert "retired" in result.output


def test_init_retired_cleanup_removes_legacy_code_review_install(configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    claude_home = hive_home.parent / ".claude"
    codex_home = hive_home.parent / ".codex"

    # Legacy on-disk layout left behind by an old `hive plugin enable code-review`.
    install_root = hive_home / "plugins" / "installed" / "code-review"
    (install_root / "skills" / "code-review").mkdir(parents=True)
    (install_root / "skills" / "code-review" / "SKILL.md").write_text("legacy\n")
    plugin_links = []
    for home in (claude_home, codex_home):
        link = home / "skills" / "code-review"
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to(install_root / "skills" / "code-review", target_is_directory=True)
        plugin_links.append(link)

    # A user-owned (non-symlink) skill listed in state must survive cleanup.
    user_skill = claude_home / "skills" / "review"
    user_skill.mkdir(parents=True)
    (user_skill / "SKILL.md").write_text("---\nname: review\ndescription: user custom\n---\n")

    state_path = hive_home / "plugins" / "state.json"
    state_path.write_text(json.dumps({
        "plugins": {
            "code-review": {
                "installRoot": str(install_root),
                "skills": [str(link) for link in plugin_links] + [str(user_skill)],
            }
        }
    }))

    removed = plugin_manager.cleanup_retired_plugins()

    assert removed == ["code-review"]
    for link in plugin_links:
        assert not link.exists() and not link.is_symlink()
    assert not install_root.exists()
    assert user_skill.is_dir() and not user_skill.is_symlink()
    assert (user_skill / "SKILL.md").read_text().startswith("---\nname: review")
    assert "code-review" not in json.loads(state_path.read_text())["plugins"]


def test_plugin_enable_notify_is_pure_toggle_without_files_or_hooks(runner, configure_hive_home):
    hive_home = configure_hive_home(tmux_inside=False)
    codex_home = hive_home.parent / ".codex"
    claude_home = hive_home.parent / ".claude"

    enabled = runner.invoke(cli, ["plugin", "enable", "notify", "--plain"])

    assert enabled.exit_code == 0
    assert "Plugin 'notify' enabled." in enabled.output
    # notify plugin is a pure toggle: the sidecar idle watcher reads its
    # enabled state. Enable installs no commands, skills, or hooks.
    assert "commands:" not in enabled.output
    assert "skills:" not in enabled.output
    assert not (claude_home / "commands" / "notify.md").exists()
    assert not (codex_home / "skills" / "notify").exists()

    settings_path = claude_home / "settings.json"
    settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
    assert "hooks" not in settings


def test_plugin_enable_disable_outputs_codex_restart_hint(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)

    enabled = runner.invoke(cli, ["plugin", "enable", "notify", "--plain"])
    disabled = runner.invoke(cli, ["plugin", "disable", "notify", "--plain"])

    hint = "existing Codex panes may not reload plugin settings dynamically"
    assert enabled.exit_code == 0
    assert disabled.exit_code == 0
    assert hint in enabled.output
    assert hint in disabled.output


@pytest.fixture
def capture_exec(monkeypatch):
    calls: list[tuple[str, list[str]]] = []

    def fake_execvp(file: str, argv: list[str]) -> None:
        calls.append((file, list(argv)))
        raise SystemExit(0)

    monkeypatch.setattr("hive.cli.os.execvp", fake_execvp)
    return calls


@pytest.mark.parametrize("hive_command, expected_mode", [("cvim", "cvim"), ("vim", "vim")])
def test_cvim_cli_exec_core_binary_with_mode(
    runner,
    configure_hive_home,
    capture_exec,
    hive_command,
    expected_mode,
):
    configure_hive_home(tmux_inside=True)

    result = runner.invoke(cli, [hive_command, "--", "--extra", "arg1"])
    assert result.exit_code == 0, result.output

    assert len(capture_exec) == 1
    command_name, argv = capture_exec[0]
    assert command_name == "bash"
    assert argv[0] == "bash"
    assert argv[1].endswith("core_assets/cvim/bin/cvim-command")
    assert argv[2] == expected_mode
    assert argv[3:] == ["--extra", "arg1"]


@pytest.fixture
def capture_fork_subprocess(monkeypatch):
    popen_calls: list[list[str]] = []
    run_calls: list[list[str]] = []

    class _FakePopen:
        def __init__(self, argv, **kwargs):
            popen_calls.append(list(argv))

    def _fake_run(argv, **kwargs):
        run_calls.append(list(argv))

        class _R:
            returncode = 0
        return _R()

    monkeypatch.setattr("hive.cli.subprocess.Popen", _FakePopen)
    monkeypatch.setattr("hive.cli.subprocess.run", _fake_run)
    return popen_calls, run_calls


@pytest.mark.parametrize("hive_command, expected_split", [("vfork", "v"), ("hfork", "h")])
def test_fork_split_spawns_background_hive_fork(
    runner, configure_hive_home, capture_fork_subprocess, monkeypatch, hive_command, expected_split
):
    configure_hive_home(tmux_inside=True)
    # the reply pane is the resolved current pane (thread-aware in codex tool
    # envs), not a raw env read
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%42")

    popen_calls, run_calls = capture_fork_subprocess
    result = runner.invoke(cli, [hive_command, "--", "--extra", "arg1"])
    assert result.exit_code == 0, result.output

    assert popen_calls == [["hive", "fork", "-s", expected_split, "--extra", "arg1"]]
    assert len(run_calls) == 1
    escape_argv = run_calls[0]
    assert escape_argv[:3] == ["tmux", "run-shell", "-b"]
    assert "%42" in escape_argv[3]
    assert "Escape" in escape_argv[3]


def test_fork_split_without_tmux_pane_skips_escape(
    runner, configure_hive_home, capture_fork_subprocess, monkeypatch
):
    configure_hive_home(tmux_inside=True)
    monkeypatch.delenv("TMUX_PANE", raising=False)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: None)

    popen_calls, run_calls = capture_fork_subprocess
    result = runner.invoke(cli, ["vfork"])
    assert result.exit_code == 0, result.output

    assert popen_calls == [["hive", "fork", "-s", "v"]]
    assert run_calls == []
