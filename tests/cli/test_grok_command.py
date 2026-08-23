"""CLI tests for `hive grok` managed launch, `hgrok` shell-init and resume-hint."""
import os
import re
import shlex
import shutil
import subprocess
import uuid
from pathlib import Path

import pytest

from hive.adapters import grok_leader
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


def _managed_env(monkeypatch, tmp_path, *, in_tmux=True, pane="%9", daemon_ok=True) -> Path:
    """Managed-launch environment; returns the pane's leader socket path."""
    monkeypatch.setenv("GROK_HOME", str(tmp_path / ".grok"))
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: in_tmux)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: pane)
    if pane:
        monkeypatch.setenv("TMUX_PANE", pane)
    else:
        monkeypatch.delenv("TMUX_PANE", raising=False)
    monkeypatch.setattr(
        "hive.adapters.grok_leader.spawn_daemon", lambda _pane: daemon_ok
    )
    return tmp_path / ".grok" / "hive" / "p9.sock"


def test_grok_bare_in_tmux_attaches_leader_and_mints_session(runner, monkeypatch, tmp_path):
    calls = _capture_exec(monkeypatch)
    sock = _managed_env(monkeypatch, tmp_path)
    runner.invoke(cli, ["grok"])
    argv = calls[0]
    assert argv[:6] == ["grok", "grok", "--leader", "--leader-socket", str(sock), "--session-id"]
    assert len(argv) == 7
    assert uuid.UUID(argv[6]).version == 4


def test_grok_records_minted_session_for_the_pane(runner, monkeypatch, tmp_path):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path)
    runner.invoke(cli, ["grok"])
    minted = calls[0][6]
    assert grok_leader.read_pane_session("%9") == (minted, os.getcwd())


def test_grok_forwards_prompt_after_injected_flags(runner, monkeypatch, tmp_path):
    calls = _capture_exec(monkeypatch)
    sock = _managed_env(monkeypatch, tmp_path)
    runner.invoke(cli, ["grok", "fix the bug"])
    argv = calls[0]
    assert str(sock) in argv
    assert "--session-id" in argv
    assert argv[-1] == "fix the bug"


def test_grok_model_flag_is_an_interactive_launch(runner, monkeypatch, tmp_path):
    calls = _capture_exec(monkeypatch)
    sock = _managed_env(monkeypatch, tmp_path)
    runner.invoke(cli, ["grok", "-m", "grok-4.6"])
    argv = calls[0]
    assert str(sock) in argv
    assert argv[-2:] == ["-m", "grok-4.6"]


def test_grok_keeps_user_session_id_and_records_it(runner, monkeypatch, tmp_path):
    # grok rejects a duplicated --session-id; the user's id is the pane's id
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path)
    runner.invoke(cli, ["grok", "--session-id", "user-sid"])
    argv = calls[0]
    assert argv.count("--session-id") == 1
    assert argv[-2:] == ["--session-id", "user-sid"]
    assert grok_leader.read_pane_session("%9") == ("user-sid", os.getcwd())


def test_grok_resume_does_not_get_a_minted_session_id(runner, monkeypatch, tmp_path):
    # `--session-id` is only valid with --resume when --fork-session names a
    # new session; minting one here would make grok reject the launch
    calls = _capture_exec(monkeypatch)
    sock = _managed_env(monkeypatch, tmp_path)
    runner.invoke(cli, ["grok", "--resume", "old-sid"])
    assert calls[0] == [
        "grok", "grok", "--leader", "--leader-socket", str(sock),
        "--resume", "old-sid",
    ]
    assert grok_leader.read_pane_session("%9") == ("old-sid", os.getcwd())


def test_grok_fork_session_mints_the_new_session_id(runner, monkeypatch, tmp_path):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path)
    runner.invoke(cli, ["grok", "--resume", "old-sid", "--fork-session"])
    argv = calls[0]
    minted = argv[argv.index("--session-id") + 1]
    assert uuid.UUID(minted).version == 4
    assert minted != "old-sid"
    assert argv[-3:] == ["--resume", "old-sid", "--fork-session"]
    assert grok_leader.read_pane_session("%9") == (minted, os.getcwd())


@pytest.mark.parametrize(
    "subcommand", ["agent", "sessions", "leader", "plugin", "mcp", "doctor", "export"]
)
def test_grok_passthrough_subcommand_runs_raw(runner, monkeypatch, tmp_path, subcommand):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path)
    monkeypatch.setattr(
        "hive.adapters.grok_leader.spawn_daemon",
        lambda _pane: pytest.fail(f"spawn_daemon must not run for {subcommand}"),
    )
    runner.invoke(cli, ["grok", subcommand, "--flag"])
    assert calls == [["grok", "grok", subcommand, "--flag"]]


@pytest.mark.parametrize("flag", ["-h", "--help", "-V", "--version"])
def test_grok_noninteractive_flags_run_raw(runner, monkeypatch, tmp_path, flag):
    # --help/--version never start a session: no daemon, grok's own output
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path)
    monkeypatch.setattr(
        "hive.adapters.grok_leader.spawn_daemon",
        lambda _pane: pytest.fail(f"spawn_daemon must not run for {flag}"),
    )
    result = runner.invoke(cli, ["grok", flag])
    assert calls == [["grok", "grok", flag]]
    assert "Usage: cli grok" not in result.output


def test_grok_outside_tmux_runs_raw(runner, monkeypatch, tmp_path):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path, in_tmux=False)
    runner.invoke(cli, ["grok", "hello"])
    assert calls == [["grok", "grok", "hello"]]


def test_grok_falls_back_to_raw_when_daemon_fails(runner, monkeypatch, tmp_path):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path, daemon_ok=False)
    result = runner.invoke(cli, ["grok"])
    assert calls == [["grok", "grok"]]
    assert result.stderr.strip().count("\n") == 0  # one warning line, no traceback dump
    assert "grok leader" in result.stderr
    assert grok_leader.read_pane_session("%9") is None  # nothing recorded for a raw run


def test_grok_raw_fallback_clears_a_stale_session_record(runner, monkeypatch, tmp_path):
    # a raw grok drives whatever session it likes: leaving the pane's previous
    # record behind would have hive resolve that stale id as this pane's
    _capture_exec(monkeypatch)
    _managed_env(monkeypatch, tmp_path, daemon_ok=False)
    grok_leader.write_pane_session("%9", "old-sid", os.getcwd())

    runner.invoke(cli, ["grok"])

    assert grok_leader.read_pane_session("%9") is None


def test_grok_keeps_no_help_option():
    assert cli.commands["grok"].add_help_option is False


# --- shell-init (hgrok) ------------------------------------------------------


def test_shell_init_zsh_emits_hgrok_launcher(runner):
    out = runner.invoke(cli, ["shell-init", "zsh"]).output
    # function form: immune to alias expansion of the name (user aliases)
    assert "function hgrok {" in out
    assert 'if hive grok "$@"; then _hive_rc=0; else _hive_rc=$?; fi' in out
    # plain `grok` keeps its own binary: no wrapper is defined for the bare name
    assert re.search(r"^function grok\b", out, re.M) is None


def test_shell_init_bash_emits_hgrok_launcher(runner):
    out = runner.invoke(cli, ["shell-init", "bash"]).output
    assert "function hgrok {" in out
    assert re.search(r"^function grok\b", out, re.M) is None


def test_shell_init_fish_emits_hgrok_launcher(runner):
    out = runner.invoke(cli, ["shell-init", "fish"]).output
    assert "function hgrok" in out
    assert "hive grok $argv" in out
    assert "if not type -q hive" in out
    assert re.search(r"^function grok\b", out, re.M) is None


def test_shell_init_posix_grok_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "zsh"]).output
    assert out.count("hive resume-hint grok 2>/dev/null") == 1


def test_shell_init_fish_grok_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "fish"]).output
    assert out.count("hive resume-hint grok 2>/dev/null") == 1


def _grok_hint_stub_bins(tmp_path):
    """Stub hive/grok that log calls: the managed launch (stub hive) exits with
    grok's own status 7 and must not trigger any second launch. The plain
    `grok` stub is the negative control — the launcher never runs it."""
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "grok").write_text(f'#!/bin/sh\necho "grok $@" >> {log}\nexit 7\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "grok" ] && exit 7\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    return log, bin_dir


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_posix_hgrok_hint_runs_after_grok_and_keeps_exit_code(runner, tmp_path, shell):
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log, bin_dir = _grok_hint_stub_bins(tmp_path)
    r = subprocess.run(
        [shell, "-c", 'eval "$HIVE_SHELL_INIT"; hgrok hello; echo "rc=$?"; true'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    # the launcher IS the grok run (exits 7); the hint follows with no second
    # launch in between
    assert log.read_text().splitlines() == ["hive grok hello", "hive resume-hint grok"]
    assert "rc=7" in r.stdout  # the hint call must not eat grok's exit code


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
def test_shell_init_fish_hgrok_hint_runs_after_grok_and_keeps_exit_code(runner, tmp_path):
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    log, bin_dir = _grok_hint_stub_bins(tmp_path)
    r = subprocess.run(
        ["fish", "-c",
         f'source {shlex.quote(str(script_file))}; hgrok hello; echo "rc=$status"; true'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    assert log.read_text().splitlines() == ["hive grok hello", "hive resume-hint grok"]
    assert "rc=7" in r.stdout


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_posix_hgrok_without_hive_on_path_returns_127(runner, tmp_path, shell):
    # the launcher is hive's entry point, not a grok wrapper: with hive gone it
    # must say so and fail, never silently start a plain grok
    shell_bin = shutil.which(shell)
    if shell_bin is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log, bin_dir = _grok_hint_stub_bins(tmp_path)
    (bin_dir / "hive").unlink()
    r = subprocess.run(
        [shell_bin, "-c", 'eval "$HIVE_SHELL_INIT"; hgrok hello; echo "rc=$?"'],
        env={"HIVE_SHELL_INIT": script, "PATH": str(bin_dir)},
        capture_output=True, text=True, timeout=15)
    assert "rc=127" in r.stdout
    assert "hgrok: hive is not on PATH" in r.stderr
    assert not log.exists()  # no fallback launch of the plain grok binary


# --- resume-hint (grok) ------------------------------------------------------


def _grok_hint_env(monkeypatch, tmp_path, *, pane=None, tagged=True):
    monkeypatch.setenv("GROK_HOME", str(tmp_path / ".grok"))
    monkeypatch.chdir(tmp_path)
    if pane is None:
        monkeypatch.delenv("TMUX_PANE", raising=False)
    else:
        monkeypatch.setenv("TMUX_PANE", pane)
    tags = {"hive-team": "t1", "hive-agent": "worker"} if tagged else {}
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _p, key: tags.get(key))


def test_grok_resume_hint_prints_cd_ready_resume(runner, monkeypatch, tmp_path):
    _grok_hint_env(monkeypatch, tmp_path, pane="%77")
    grok_leader.write_pane_session("%77", "sid-42", str(tmp_path))
    result = runner.invoke(cli, ["resume-hint", "grok"])
    assert result.exit_code == 0
    assert result.output.splitlines() == [
        "Resume from anywhere:",
        f"  cd {shlex.quote(os.getcwd())} && hive grok --resume sid-42",
    ]


def test_grok_resume_hint_without_session_file_yields_no_hint(runner, monkeypatch, tmp_path):
    _grok_hint_env(monkeypatch, tmp_path, pane="%77")
    result = runner.invoke(cli, ["resume-hint", "grok"])
    assert result.exit_code == 0
    assert result.output == ""


def test_grok_resume_hint_untagged_pane_prints_nothing(runner, monkeypatch, tmp_path):
    # the team gate is shared by every CLI: any tmux user gets a per-pane
    # leader from the managed launch, so a recorded session alone must not
    # qualify a pane for a hint
    _grok_hint_env(monkeypatch, tmp_path, pane="%77", tagged=False)
    grok_leader.write_pane_session("%77", "sid-42", str(tmp_path))

    def _must_not_call(_p):
        raise AssertionError("session lookup must not run for an untagged pane")

    monkeypatch.setattr("hive.adapters.grok_leader.read_pane_session", _must_not_call)
    result = runner.invoke(cli, ["resume-hint", "grok"])
    assert result.exit_code == 0
    assert result.output == ""


def test_grok_resume_hint_no_pane_yields_no_hint(runner, monkeypatch, tmp_path):
    _grok_hint_env(monkeypatch, tmp_path)  # TMUX_PANE removed

    def _must_not_call(_p):
        raise AssertionError("session lookup must not run without a pane")

    monkeypatch.setattr("hive.adapters.grok_leader.read_pane_session", _must_not_call)
    result = runner.invoke(cli, ["resume-hint", "grok"])
    assert result.exit_code == 0
    assert result.output == ""


@pytest.mark.parametrize("evil_id", [
    "ok\x1b]52;c;AAAA\x07",  # OSC 52 clipboard write, ESC + BEL
    "--yolo",                # option-shaped id
])
def test_grok_resume_hint_recorded_id_untrusted_gates(runner, monkeypatch, tmp_path, evil_id):
    # hive wrote the file, but its content is still untrusted for printing
    _grok_hint_env(monkeypatch, tmp_path, pane="%77")
    grok_leader.write_pane_session("%77", evil_id, str(tmp_path))
    result = runner.invoke(cli, ["resume-hint", "grok"])
    assert result.exit_code == 0
    assert result.output == ""


# --- init gate: a plain grok pane (no pane leader) must relaunch via hgrok ---

import hive.cli as cli_mod


def _profile(name):
    return type("P", (), {"name": name})()


def test_init_gate_allows_leader_backed_grok(monkeypatch, tmp_path):
    sock = tmp_path / "p1.sock"
    sock.touch()
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _p: _profile("grok"))
    monkeypatch.setattr("hive.adapters.grok_leader.pane_socket_path", lambda _p: sock)
    monkeypatch.setattr("hive.adapters.grok_leader.probe_socket", lambda _s: True)
    cli_mod._require_daemon_backed("%1")  # leader answers -> allowed


def test_init_gate_blocks_plain_grok_and_points_at_resume(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _p: _profile("grok"))
    monkeypatch.setattr(
        "hive.adapters.grok_leader.pane_socket_path", lambda _p: tmp_path / "missing.sock"
    )
    monkeypatch.setattr("hive.adapters.grok_leader.session_id_for_pane", lambda _p: "sid-77")
    with pytest.raises(SystemExit):
        cli_mod._require_daemon_backed("%1")
    err = capsys.readouterr().err
    assert "hive grok --resume sid-77" in err
    assert "hgrok" in err


def test_init_gate_blocks_plain_grok_without_recorded_session(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _p: _profile("grok"))
    monkeypatch.setattr(
        "hive.adapters.grok_leader.pane_socket_path", lambda _p: tmp_path / "missing.sock"
    )
    monkeypatch.setattr("hive.adapters.grok_leader.session_id_for_pane", lambda _p: None)
    with pytest.raises(SystemExit):
        cli_mod._require_daemon_backed("%1")
    assert "run: hive grok\n" in capsys.readouterr().err
