"""CLI tests for `hive codex` managed launch and `hive shell-init`."""
import json
import os
import re
import shlex
import shutil
import subprocess
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


# --- resume-hint (codex) -----------------------------------------------------


def _fake_rollout(codex_home, day, stem, session_id, cwd, mtime):
    d = codex_home / "sessions" / "2026" / "07" / day
    d.mkdir(parents=True, exist_ok=True)
    p = d / f"{stem}.jsonl"
    p.write_text(json.dumps({
        "type": "session_meta",
        "timestamp": "2026-07-13T00:00:00.000Z",
        "payload": {"id": session_id, "cwd": cwd},
    }) + "\n")
    os.utime(p, (mtime, mtime))
    return p


def _codex_hint_env(monkeypatch, tmp_path, *, pane=None, tagged=True):
    home = tmp_path / "codex-home"
    monkeypatch.setenv("CODEX_HOME", str(home))
    monkeypatch.chdir(tmp_path)
    if pane is None:
        monkeypatch.delenv("TMUX_PANE", raising=False)
    else:
        monkeypatch.setenv("TMUX_PANE", pane)
    tags = {"hive-team": "t1", "hive-agent": "validator"} if tagged else {}
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option", lambda _p, key: tags.get(key)
    )
    return home


def test_shell_init_posix_codex_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "zsh"]).output
    assert 'hive resume-hint codex 2>/dev/null' in out
    assert out.count('hive resume-hint codex 2>/dev/null') == 1


def test_shell_init_fish_codex_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "fish"]).output
    assert "hive resume-hint codex 2>/dev/null" in out
    assert out.count("hive resume-hint codex 2>/dev/null") == 1


def _codex_hint_stub_bins(tmp_path):
    """Stub hive/codex that log calls: managed launch fails (exit 1) so the
    function falls back to raw codex, which exits 7."""
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text(f'#!/bin/sh\necho "codex $@" >> {log}\nexit 7\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "codex" ] && exit 1\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    return log, bin_dir


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_posix_codex_hint_runs_after_codex_and_keeps_exit_code(runner, tmp_path, shell):
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log, bin_dir = _codex_hint_stub_bins(tmp_path)
    r = subprocess.run(
        [shell, "-c",
         'eval "$HIVE_SHELL_INIT"; codex hello; echo "rc=$?"; codex --help; true'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive codex hello"  # managed attempt (stub exits 1)
    assert lines[1] == "codex hello"  # raw fallback (stub exits 7)
    assert lines[2] == "hive resume-hint codex"
    assert lines[3] == "codex --help"  # passthrough stays raw and hint-free
    assert len(lines) == 4
    assert "rc=7" in r.stdout  # the hint call must not eat codex's exit code


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
def test_shell_init_fish_codex_hint_runs_after_codex_and_keeps_exit_code(runner, tmp_path):
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    log, bin_dir = _codex_hint_stub_bins(tmp_path)
    r = subprocess.run(
        ["fish", "-c",
         f'source {shlex.quote(str(script_file))}; '
         'codex hello; echo "rc=$status"; codex --help; true'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
             "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive codex hello"
    assert lines[1] == "codex hello"
    assert lines[2] == "hive resume-hint codex"
    assert lines[3] == "codex --help"
    assert len(lines) == 4
    assert "rc=7" in r.stdout


def test_codex_resume_hint_daemon_unresolved_yields_no_hint(runner, monkeypatch, tmp_path):
    # no daemon answer means no hint: a transcript scan would reintroduce a
    # second source of truth (a fresh rollout in cwd proves nothing is scanned)
    home = _codex_hint_env(monkeypatch, tmp_path, pane="%77")
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", lambda _p: None
    )
    _fake_rollout(home, "01", "rollout", "scan-id", os.getcwd(), 2000.0)
    result = runner.invoke(cli, ["resume-hint", "codex"])
    assert result.exit_code == 0
    assert result.output == ""


def test_codex_resume_hint_no_pane_yields_no_hint(runner, monkeypatch, tmp_path):
    home = _codex_hint_env(monkeypatch, tmp_path)  # TMUX_PANE removed

    def _must_not_call(_p):
        raise AssertionError("daemon lookup must not run without a pane")

    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", _must_not_call
    )
    _fake_rollout(home, "01", "rollout", "scan-id", os.getcwd(), 2000.0)
    result = runner.invoke(cli, ["resume-hint", "codex"])
    assert result.exit_code == 0
    assert result.output == ""


@pytest.mark.parametrize("evil_id", [
    "ok\x1b]52;c;AAAA\x07",  # OSC 52 clipboard write, ESC + BEL
    "--dangerously-bypass-approvals-and-sandbox",  # option-shaped id
])
def test_codex_resume_hint_daemon_id_untrusted_gates(runner, monkeypatch, tmp_path, evil_id):
    # authority or not, the id is untrusted for printing
    _codex_hint_env(monkeypatch, tmp_path, pane="%77")
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", lambda _p: evil_id
    )
    result = runner.invoke(cli, ["resume-hint", "codex"])
    assert result.exit_code == 0
    assert result.output == ""


def test_codex_resume_hint_untagged_pane_prints_nothing(runner, monkeypatch, tmp_path):
    # the team gate is shared by both CLIs: any tmux user gets a per-pane
    # daemon from the managed launch, so daemon reachability alone must not
    # qualify a pane for a hint
    _codex_hint_env(monkeypatch, tmp_path, pane="%77", tagged=False)

    def _must_not_call(_p):
        raise AssertionError("daemon lookup must not run for an untagged pane")

    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", _must_not_call
    )
    result = runner.invoke(cli, ["resume-hint", "codex"])
    assert result.exit_code == 0
    assert result.output == ""
