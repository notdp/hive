"""CLI tests for `hive claude` managed launch and its shell-init function."""
import pytest

from hive.cli import cli

pytestmark = pytest.mark.cli

_FLAGS = ["--channels", "plugin:hive-channel@hive"]


class _ExecCalled(Exception):
    """Sentinel: os.execvp never returns in production; stop the test here."""


def _capture_exec(monkeypatch) -> list[list[str]]:
    calls: list[list[str]] = []

    def _fake_execvp(file: str, argv: list[str]) -> None:
        calls.append([file, *list(argv)])
        raise _ExecCalled()

    monkeypatch.setattr("hive.cli.os.execvp", _fake_execvp)
    return calls


def _managed_env(monkeypatch, *, in_tmux=True, flags=None, pane="%99"):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: in_tmux)
    if pane:
        monkeypatch.setenv("TMUX_PANE", pane)
    else:
        monkeypatch.delenv("TMUX_PANE", raising=False)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: pane or None)
    monkeypatch.setattr(
        "hive.adapters.claude_channel.prepare_pane",
        lambda _cwd: list(_FLAGS) if flags is None else list(flags),
    )
    cleared: list[str] = []
    monkeypatch.setattr("hive.adapters.claude_channel.clear_ready", cleared.append)
    return cleared


def test_claude_bare_in_tmux_appends_channel_flags(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["claude"])
    assert calls == [["claude", "claude", *_FLAGS]]


def test_claude_user_args_precede_channel_flags(runner, monkeypatch):
    # both channel flags are variadic in claude's parser: appending them after
    # the user's argv keeps a user positional prompt out of their reach
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["claude", "hello"])
    assert calls == [["claude", "claude", "hello", *_FLAGS]]


def test_claude_managed_launch_clears_stale_marker_first(runner, monkeypatch):
    # a marker left by a previous claude in this pane must not survive into
    # the new launch: readiness may only come from the new server
    calls = _capture_exec(monkeypatch)
    cleared = _managed_env(monkeypatch, pane="%99")
    runner.invoke(cli, ["claude", "hello"])
    assert cleared == ["%99"]
    assert calls  # cleared before the managed exec happened


def test_claude_passthrough_does_not_touch_marker(runner, monkeypatch):
    _capture_exec(monkeypatch)
    cleared = _managed_env(monkeypatch, pane="%99")
    runner.invoke(cli, ["claude", "agents", "--json"])
    assert cleared == []  # raw passthrough leaves pane state alone


def test_claude_failed_prepare_still_clears_stale_marker(runner, monkeypatch):
    # even when the wrapper falls back to `command claude` (exit 1), the pane
    # must not keep a stale marker claiming a channel that will not exist
    _capture_exec(monkeypatch)
    cleared = _managed_env(monkeypatch, flags=[], pane="%99")
    result = runner.invoke(cli, ["claude", "hello"])
    assert result.exit_code != 0
    assert cleared == ["%99"]


def test_claude_passthrough_subcommand_runs_raw(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["claude", "agents", "--json"])
    assert calls == [["claude", "claude", "agents", "--json"]]


@pytest.mark.parametrize("flag", ["-p", "--print", "--help", "--version"])
def test_claude_noninteractive_flags_run_raw(runner, monkeypatch, flag):
    # -p/--print sessions would hard-block on the dev-channel consent gate;
    # --help/--version never start a session
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["claude", flag, "say hi"])
    assert calls == [["claude", "claude", flag, "say hi"]]


def test_claude_outside_tmux_runs_raw(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, in_tmux=False)
    runner.invoke(cli, ["claude", "hello"])
    assert calls == [["claude", "claude", "hello"]]


def test_claude_exits_nonzero_when_channel_unavailable(runner, monkeypatch):
    # nonzero exit lets the shell function fall back to `command claude`
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, flags=[])
    result = runner.invoke(cli, ["claude", "hello"])
    assert result.exit_code != 0
    assert calls == []  # neither managed nor raw exec: the wrapper decides


def test_shell_init_zsh_emits_claude_function(runner):
    result = runner.invoke(cli, ["shell-init", "zsh"])
    assert result.exit_code == 0
    assert "codex() {" in result.output
    assert "claude() {" in result.output
    assert "hive claude \"$@\" || command claude \"$@\"" in result.output
    # passthrough guards present for both surfaces
    assert "agents" in result.output
    assert "--print" in result.output


def test_shell_init_fish_emits_claude_function(runner):
    result = runner.invoke(cli, ["shell-init", "fish"])
    assert result.exit_code == 0
    assert "function codex" in result.output
    assert "function claude" in result.output
    assert "hive claude $argv; or command claude $argv" in result.output
