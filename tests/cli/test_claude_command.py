"""CLI tests for `hive claude` managed launch and its shell-init function."""
import json
import os
import re
import shlex
import shutil
import subprocess

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


def test_claude_leading_flag_before_subcommand_runs_raw(runner, monkeypatch):
    # ghost-session regression: `alias claude='claude --verbose'` prepends a
    # flag, and a leading flag also defeats claude's own subcommand dispatch —
    # a managed exec would turn "daemon status" into a prompt
    calls = _capture_exec(monkeypatch)
    cleared = _managed_env(monkeypatch, pane="%99")
    runner.invoke(cli, ["claude", "--verbose", "daemon", "status"])
    assert calls == [["claude", "claude", "--verbose", "daemon", "status"]]
    assert cleared == []  # the pane's live channel marker survives


def test_claude_hidden_subcommand_runs_raw(runner, monkeypatch):
    # daemon/attach/logs/stop/remote-control are absent from `claude --help`
    # but real; a missing allowlist entry turns them into ghost sessions
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["claude", "daemon", "status"])
    assert calls == [["claude", "claude", "daemon", "status"]]


def test_claude_flag_value_is_not_a_subcommand(runner, monkeypatch):
    # `--agent doctor`: "doctor" is the flag's value, not a subcommand
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["claude", "--agent", "doctor"])
    assert calls == [["claude", "claude", "--agent", "doctor", *_FLAGS]]


def test_claude_channel_server_dispatch_runs_server_main(runner, monkeypatch):
    # published plugin manifests invoke `hive claude channel-server`; the
    # exact argv must run the MCP server in-process, never exec claude
    calls = _capture_exec(monkeypatch)
    served: list[bool] = []
    monkeypatch.setattr(
        "hive.adapters.claude_channel_server.main", lambda: served.append(True)
    )
    result = runner.invoke(cli, ["claude", "channel-server"])
    assert result.exit_code == 0
    assert served == [True]
    assert calls == []


def test_claude_channel_server_with_extra_args_is_not_dispatch(runner, monkeypatch):
    # anything beyond the exact internal argv stays on the passthrough path
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)

    def _fail() -> None:
        raise AssertionError("server main must not run for non-exact argv")

    monkeypatch.setattr("hive.adapters.claude_channel_server.main", _fail)
    runner.invoke(cli, ["claude", "channel-server", "--verbose"])
    assert calls == [["claude", "claude", "channel-server", "--verbose", *_FLAGS]]


def test_claude_double_dash_positional_is_a_prompt(monkeypatch):
    # `claude -- daemon` explicitly makes "daemon" a prompt, not a subcommand.
    # Exercised on _exec_claude_managed directly: click strips the first `--`
    # from `hive claude -- daemon` before ctx.args, so via the CLI this branch
    # only sees what the shell wrapper forwards without a click layer.
    from hive.cli import _exec_claude_managed

    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    with pytest.raises(_ExecCalled):
        _exec_claude_managed(["--", "daemon"])
    assert calls == [["claude", "claude", "--", "daemon", *_FLAGS]]


@pytest.mark.parametrize("flag", ["-p", "--print", "--help", "--version"])
def test_claude_noninteractive_flags_run_raw(runner, monkeypatch, flag):
    # a -p/--print run has no interactive session for hive to message;
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


def test_claude_exits_127_when_channel_unavailable(runner, monkeypatch):
    # the reserved wrapper-declined code lets the shell function fall back to
    # `command claude`; any other status is claude's own exit
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, flags=[])
    result = runner.invoke(cli, ["claude", "hello"])
    assert result.exit_code == 127
    assert calls == []  # neither managed nor raw exec: the wrapper decides


def test_shell_init_zsh_emits_claude_function(runner):
    result = runner.invoke(cli, ["shell-init", "zsh"])
    assert result.exit_code == 0
    # ksh-style `function name {` bypasses alias expansion of the name in
    # both zsh and bash, so pre-existing user aliases cannot break the parse
    assert "function codex {" in result.output
    assert "function claude {" in result.output
    assert 'hive claude "$@"' in result.output
    # raw fallback only on the reserved wrapper-declined code
    assert 'if [ "$_hive_rc" -eq 127 ]; then' in result.output
    # passthrough guards present for both surfaces
    assert "agents" in result.output
    assert "--print" in result.output


def test_shell_init_bash_emits_function_form(runner):
    result = runner.invoke(cli, ["shell-init", "bash"])
    assert result.exit_code == 0
    assert "function codex {" in result.output
    assert "function claude {" in result.output


@pytest.mark.skipif(shutil.which("bash") is None, reason="bash not available")
def test_shell_init_bash_survives_existing_aliases(runner):
    # bash alias-expands function-definition names too when expand_aliases is
    # on (interactive/bashrc); the function form must survive it
    script = runner.invoke(cli, ["shell-init", "bash"]).output
    r = subprocess.run(
        ["bash", "-c",
         "shopt -s expand_aliases; "
         "alias claude='claude --verbose'; alias codex='codex -q'; "
         'eval "$HIVE_SHELL_INIT" && declare -F claude codex >/dev/null '
         '&& echo OK'],
        env={**os.environ, "HIVE_SHELL_INIT": script},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip() == "OK"


@pytest.mark.skipif(shutil.which("zsh") is None, reason="zsh not available")
def test_shell_init_zsh_survives_existing_claude_alias(runner):
    # regression: sourcing the integration with a pre-existing alias used to
    # abort with "defining function based on alias `claude'" + parse error
    script = runner.invoke(cli, ["shell-init", "zsh"]).output
    # the script rides in an env var so its own quotes survive untouched
    r = subprocess.run(
        ["zsh", "-c",
         "alias claude='claude --verbose'; alias codex='codex -q'; "
         'eval "$HIVE_SHELL_INIT" && print -r -- "$+functions[claude] $+functions[codex]"'],
        env={**os.environ, "HIVE_SHELL_INIT": script},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip() == "1 1"  # both functions defined despite aliases


def test_shell_init_fish_emits_claude_function(runner):
    result = runner.invoke(cli, ["shell-init", "fish"])
    assert result.exit_code == 0
    assert "function codex" in result.output
    assert "function claude" in result.output
    assert "hive claude $argv" in result.output
    assert "if test $_hive_rc -eq 127" in result.output


@pytest.mark.skipif(shutil.which("zsh") is None, reason="zsh not available")
def test_shell_init_zsh_routes_aliased_subcommand_raw(runner, tmp_path):
    # end-to-end ghost-session regression: with a user alias injecting a
    # leading flag, `claude daemon status` must reach the real binary — not
    # `hive claude`, whose managed exec would spawn an interactive session
    # with "daemon status" as the prompt
    script = runner.invoke(cli, ["shell-init", "zsh"]).output
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\n')
    (bin_dir / "hive").write_text(f'#!/bin/sh\necho "hive $@" >> {log}\n')
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    # the trailing eval forces a fresh parse so the alias actually expands:
    # zsh parses the whole -c string before the alias command runs
    r = subprocess.run(
        ["zsh", "-c",
         "alias claude='claude --verbose'; "
         'eval "$HIVE_SHELL_INIT"; eval \'claude daemon status\''],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    assert log.read_text() == "claude --verbose daemon status\n"


# --- resume-hint (claude) -----------------------------------------------------


def test_shell_init_posix_claude_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "zsh"]).output
    assert 'hive resume-hint claude 2>/dev/null' in out
    assert "return $_hive_rc" in out
    # claude only: the codex function stays hint-free
    assert out.count("hive resume-hint claude 2>/dev/null") == 1


def test_shell_init_fish_claude_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "fish"]).output
    assert "hive resume-hint claude 2>/dev/null" in out
    assert "return $_hive_rc" in out
    assert out.count("hive resume-hint claude 2>/dev/null") == 1


def _hint_stub_bins(tmp_path):
    """Stub hive/claude that log calls: the managed launch declines with the
    reserved 127, so the function falls back to raw claude, which exits 7."""
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\nexit 7\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "claude" ] && exit 127\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    return log, bin_dir


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_posix_hint_runs_after_claude_and_keeps_exit_code(runner, tmp_path, shell):
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log, bin_dir = _hint_stub_bins(tmp_path)
    r = subprocess.run(
        [shell, "-c",
         'eval "$HIVE_SHELL_INIT"; claude hello; echo "rc=$?"; claude --help; true'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"  # managed attempt (stub declines: 127)
    assert lines[1] == "claude hello"  # raw fallback on 127 (stub exits 7)
    assert lines[2] == "hive resume-hint claude"
    assert lines[3] == "claude --help"  # passthrough stays raw and hint-free
    assert len(lines) == 4
    assert "rc=7" in r.stdout  # the hint call must not eat claude's exit code


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
def test_shell_init_fish_script_parses(runner, tmp_path):
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    r = subprocess.run(["fish", "-n", str(script_file)],
                       capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
def test_shell_init_fish_hint_runs_after_claude_and_keeps_exit_code(runner, tmp_path):
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    log, bin_dir = _hint_stub_bins(tmp_path)
    r = subprocess.run(
        ["fish", "-c",
         f'source {shlex.quote(str(script_file))}; '
         'claude hello; echo "rc=$status"; claude --help; true'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
             "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"
    assert lines[1] == "claude hello"
    assert lines[2] == "hive resume-hint claude"
    assert lines[3] == "claude --help"
    assert len(lines) == 4
    assert "rc=7" in r.stdout

def _fake_snapshot(hive_home, team, members):
    d = hive_home / "state" / "resume"
    d.mkdir(parents=True, exist_ok=True)
    (d / f"{team}.json").write_text(json.dumps({
        "schema": 1, "handle": team, "team": team, "group": "duo",
        "windowName": "", "workspace": "", "repoCwd": "", "repo": "",
        "branch": "", "pr": "", "createdAt": "1", "savedAt": "now",
        "members": members,
    }))


def _member_pane_env(monkeypatch, tmp_path, *, pane="%5", team="t1", agent="worker"):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / "hive-home"))
    monkeypatch.setenv("TMUX_PANE", pane)
    tags = {"hive-team": team, "hive-agent": agent}
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option", lambda _p, key: tags.get(key)
    )
    return tmp_path / "hive-home"


def test_resume_hint_reads_team_snapshot_and_quotes(runner, monkeypatch, tmp_path):
    # the sidecar already records each member's sessionId into the resume
    # store and the entry survives the pane's process exiting — the hint
    # reads that, never the filesystem; cwd and id are still quoted
    work = tmp_path / "wo rk"
    work.mkdir()
    monkeypatch.chdir(work)
    home = _member_pane_env(monkeypatch, tmp_path)
    evil_id = "id; rm -rf ~"
    _fake_snapshot(home, "t1", [
        {"name": "validator", "cli": "codex", "sessionId": "other"},
        {"name": "worker", "cli": "claude", "sessionId": evil_id},
    ])
    cwd = os.getcwd()
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == (
        "Resume from anywhere:\n"
        f"  cd {shlex.quote(cwd)} && claude --resume {shlex.quote(evil_id)}\n"
    )


def test_resume_hint_untagged_pane_prints_nothing(runner, monkeypatch, tmp_path):
    # not a team member: tracking arbitrary user panes is not this feature's
    # job, so there is nothing to suggest
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / "hive-home"))
    monkeypatch.setenv("TMUX_PANE", "%5")
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _p, _k: None)
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == ""


def test_resume_hint_no_pane_env_prints_nothing(runner, monkeypatch, tmp_path):
    monkeypatch.delenv("TMUX_PANE", raising=False)
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == ""


def test_resume_hint_missing_snapshot_or_member_prints_nothing(runner, monkeypatch, tmp_path):
    home = _member_pane_env(monkeypatch, tmp_path)
    r_no_snap = runner.invoke(cli, ["resume-hint", "claude"])
    _fake_snapshot(home, "t1", [{"name": "someone-else", "sessionId": "x"}])
    r_no_member = runner.invoke(cli, ["resume-hint", "claude"])
    _fake_snapshot(home, "t1", [{"name": "worker", "sessionId": ""}])
    r_no_session = runner.invoke(cli, ["resume-hint", "claude"])
    for r in (r_no_snap, r_no_member, r_no_session):
        assert r.exit_code == 0
        assert r.output == ""


@pytest.mark.parametrize("evil_id", [
    "ok\x1b]52;c;AAAA\x07",   # OSC 52 clipboard write, ESC + BEL
    "ok\nrm -rf ~",           # newline splits the printed command line
    "--dangerously-skip-permissions",  # option-shaped id
])
def test_resume_hint_snapshot_id_untrusted_gates(runner, monkeypatch, tmp_path, evil_id):
    # snapshot content is still untrusted for printing: quoting protects a
    # later shell parse, not the automatic print, and a leading-dash id
    # would parse as a flag (`claude --resume` takes an optional value)
    home = _member_pane_env(monkeypatch, tmp_path)
    _fake_snapshot(home, "t1", [{"name": "worker", "sessionId": evil_id}])
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == ""


def test_resume_hint_control_bytes_in_cwd_silence_hint(runner, monkeypatch, tmp_path):
    evil_dir = tmp_path / "d\x1b]0;pwned\x07ir"
    evil_dir.mkdir()
    home = _member_pane_env(monkeypatch, tmp_path)
    _fake_snapshot(home, "t1", [{"name": "worker", "sessionId": "good-id"}])
    monkeypatch.chdir(evil_dir)
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == ""


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_posix_no_relaunch_on_claude_own_exit_code(runner, tmp_path, shell):
    # regression: `hive claude || command claude` used to start a SECOND raw
    # claude whenever the exec'd claude exited nonzero on its own; only the
    # reserved 127 may trigger the fallback
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\nexit 0\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "claude" ] && exit 7\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    r = subprocess.run(
        [shell, "-c", 'eval "$HIVE_SHELL_INIT"; claude hello; echo "rc=$?"'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"  # exits 7: claude's own status
    assert lines[1] == "hive resume-hint claude"  # no raw relaunch in between
    assert len(lines) == 2
    assert "rc=7" in r.stdout


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
def test_shell_init_fish_no_relaunch_on_claude_own_exit_code(runner, tmp_path):
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\nexit 0\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "claude" ] && exit 7\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    r = subprocess.run(
        ["fish", "-c",
         f'source {shlex.quote(str(script_file))}; claude hello; echo "rc=$status"'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"
    assert lines[1] == "hive resume-hint claude"
    assert len(lines) == 2
    assert "rc=7" in r.stdout
