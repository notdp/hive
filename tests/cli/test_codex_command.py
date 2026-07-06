"""CLI tests for `hive codex` managed launch and `hive shell-init`."""
import os
from pathlib import Path

import pytest

from hive.cli import cli

pytestmark = pytest.mark.cli


class _ExecCalled(Exception):
    """Sentinel: os.execvp never returns in production; stop the test here."""


def _capture_exec(monkeypatch) -> list[list[str]]:
    calls: list[list[str]] = []

    def _fake_execvp(file: str, argv: list[str]) -> None:
        calls.append([file, *list(argv)])
        raise _ExecCalled()

    monkeypatch.setattr("hive.cli.os.execvp", _fake_execvp)
    return calls


def _managed_env(monkeypatch, *, in_tmux=True, pane="%9", daemon_ok=True):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: in_tmux)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: pane)
    if pane:
        monkeypatch.setenv("TMUX_PANE", pane)
    else:
        monkeypatch.delenv("TMUX_PANE", raising=False)
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.spawn_daemon", lambda _pane: daemon_ok
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_socket_path",
        lambda _pane: Path("/tmp/ctrl/hive-pane-9.sock"),
    )


def test_codex_bare_in_tmux_binds_remote_and_cwd(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex"])
    assert calls == [[
        "codex", "codex",
        "-c", "check_for_update_on_startup=false",
        "--remote", "unix:///tmp/ctrl/hive-pane-9.sock",
        "--cd", os.getcwd(),
    ]]


def test_codex_forwards_prompt_after_injected_flags(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "fix the bug"])
    argv = calls[0]
    assert "--remote" in argv and "unix:///tmp/ctrl/hive-pane-9.sock" in argv
    assert "check_for_update_on_startup=false" in argv
    assert argv[-1] == "fix the bug"


def test_codex_resume_is_bound_to_daemon(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "resume", "sess-123"])
    assert calls[0][-2:] == ["resume", "sess-123"]
    assert "--remote" in calls[0]


def test_codex_passthrough_subcommand_runs_raw(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "exec", "echo", "hi"])
    assert calls == [["codex", "codex", "exec", "echo", "hi"]]


def test_codex_passthrough_subcommand_after_global_options(runner, monkeypatch):
    # codex allows global options before the subcommand; -c consumes its value,
    # so `exec` must still be detected as a management subcommand -> raw.
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "-c", "foo=1", "exec", "echo", "hi"])
    assert calls == [["codex", "codex", "-c", "foo=1", "exec", "echo", "hi"]]


def test_codex_model_flag_is_interactive_launch(runner, monkeypatch):
    # `-m gpt5` is a value-taking option with no subcommand -> managed launch.
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "-m", "gpt5"])
    argv = calls[0]
    assert "--remote" in argv and argv[-2:] == ["-m", "gpt5"]


def test_codex_respects_user_remote(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "--remote", "ws://host:1"])
    # raw passthrough: no second --remote, no --cd injected
    assert calls == [["codex", "codex", "--remote", "ws://host:1"]]


def test_codex_respects_user_remote_equals_form(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "--remote=unix:///tmp/user.sock"])
    assert calls == [["codex", "codex", "--remote=unix:///tmp/user.sock"]]


def test_codex_does_not_double_cwd_when_user_passes_cd(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "-C", "/work"])
    argv = calls[0]
    assert argv.count("--cd") == 0
    assert argv[-2:] == ["-C", "/work"]
    assert "--remote" in argv


def test_codex_does_not_double_cwd_equals_form(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "--cd=/work"])
    argv = calls[0]
    assert argv.count("--cd") == 0
    assert "--cd=/work" in argv
    assert "--remote" in argv


def test_codex_outside_tmux_runs_raw(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, in_tmux=False)
    runner.invoke(cli, ["codex", "hello"])
    assert calls == [["codex", "codex", "hello"]]


def test_codex_falls_back_to_raw_when_daemon_fails(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, daemon_ok=False)
    runner.invoke(cli, ["codex"])
    assert calls == [["codex", "codex"]]


def test_shell_init_zsh_emits_guarded_function(runner):
    out = runner.invoke(cli, ["shell-init", "zsh"]).output
    # function form: immune to alias expansion of the name (user aliases)
    assert "function codex {" in out
    assert 'if [ -z "$TMUX" ]; then command codex "$@"; return; fi' in out
    assert "hive codex \"$@\" || command codex \"$@\"" in out
    # management subcommands stay raw
    assert "app-server" in out and "exec" in out and "--version" in out


def test_shell_init_fish_emits_function(runner):
    out = runner.invoke(cli, ["shell-init", "fish"]).output
    assert "function codex" in out
    assert "command codex $argv" in out
    assert "hive codex $argv; or command codex $argv" in out


# --- init gate: embedded codex must relaunch daemon-backed ---

import hive.cli as cli_mod


def _profile(name):
    return type("P", (), {"name": name})()


def test_init_gate_ignores_non_codex(monkeypatch):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("claude")
    )
    cli_mod._require_codex_daemon_backed("%1")  # no raise, no daemon lookup


def test_init_gate_allows_daemon_backed_codex(monkeypatch, tmp_path):
    sock = tmp_path / "hive-pane-1.sock"
    sock.touch()
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("codex")
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_socket_path", lambda _p: sock
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.probe_socket", lambda _s: True)
    cli_mod._require_codex_daemon_backed("%1")  # daemon answers -> allowed


def test_init_gate_uses_native_marker_pane(monkeypatch, tmp_path):
    sock = tmp_path / "hive-pane-9.sock"
    sock.touch()
    seen = {}
    monkeypatch.setenv("HIVE_CODEX_PANE", "%9")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _p: (_ for _ in ()).throw(
            AssertionError("profile should not be used")
        ),
    )

    def fake_socket_path(pane):
        seen["pane"] = pane
        return sock

    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_socket_path",
        fake_socket_path,
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.probe_socket", lambda _s: True)
    cli_mod._require_codex_daemon_backed("%bad")
    assert seen["pane"] == "%9"


def test_init_gate_blocks_codex_tool_without_native_marker(monkeypatch, capsys):
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-1")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _p: (_ for _ in ()).throw(
            AssertionError("profile should not be used")
        ),
    )
    with pytest.raises(SystemExit):
        cli_mod._require_codex_daemon_backed("%wrong")
    err = capsys.readouterr().err
    assert "hive codex resume" in err
    assert "abc-123" not in err


def test_init_gate_blocks_embedded_codex_with_resume_hint(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("codex")
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_socket_path",
        lambda _p: tmp_path / "absent.sock",
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.probe_socket", lambda _s: False)
    monkeypatch.setattr(
        "hive.adapters.codex.CodexAdapter.resolve_current_session_id",
        lambda _self, _p: "abc-123",
    )
    with pytest.raises(SystemExit):
        cli_mod._require_codex_daemon_backed("%1")
    err = capsys.readouterr().err
    assert "Ctrl-C" in err
    assert "hive codex resume abc-123" in err


def test_init_gate_hint_without_sid_falls_back_to_picker(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("codex")
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_socket_path",
        lambda _p: tmp_path / "absent.sock",
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.probe_socket", lambda _s: False)
    monkeypatch.setattr(
        "hive.adapters.codex.CodexAdapter.resolve_current_session_id",
        lambda _self, _p: None,
    )
    with pytest.raises(SystemExit):
        cli_mod._require_codex_daemon_backed("%1")
    err = capsys.readouterr().err
    assert "run: hive codex resume\n" in err  # picker form, no trailing sid


def test_codex_degraded_gate_fails_every_time(monkeypatch, capsys):
    monkeypatch.setenv("CODEX_THREAD_ID", "thread/1")

    with pytest.raises(SystemExit) as first_exit:
        cli_mod._require_codex_native("team")
    first = capsys.readouterr().err
    with pytest.raises(SystemExit) as second_exit:
        cli_mod._require_codex_native("send")
    second = capsys.readouterr().err

    assert first_exit.value.code == 1
    assert second_exit.value.code == 1
    assert "hive codex resume" in first
    assert "hive codex resume" in second


def test_codex_degraded_gate_fails_user_commands_from_cli(runner, monkeypatch):
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-2")

    team_result = runner.invoke(cli, ["team"])
    send_result = runner.invoke(cli, ["send", "orch", "hi"])

    assert team_result.exit_code == 1
    assert send_result.exit_code == 1
    assert "hive codex resume" in team_result.stderr
    assert "hive codex resume" in send_result.stderr


def test_codex_degraded_gate_skips_bypass_and_native_marker(monkeypatch, capsys):
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-2")
    cli_mod._require_codex_native("wait-status")
    assert capsys.readouterr().err == ""

    monkeypatch.setenv("HIVE_CODEX_PANE", "%9")
    cli_mod._require_codex_native("team")
    assert capsys.readouterr().err == ""


def test_codex_degraded_bypass_command_reaches_own_error(runner, monkeypatch):
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-3")
    result = runner.invoke(cli, ["statuses"])
    assert result.exit_code == 1
    assert "hive codex resume" not in result.stderr
    assert "was removed" in result.stderr
