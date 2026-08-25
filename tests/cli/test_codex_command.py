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


_SHARED_SOCK = "/tmp/ctrl/hive-shared.sock"
_INJECTED = ["codex", "codex", "-c", "check_for_update_on_startup=false",
             "--remote", f"unix://{_SHARED_SOCK}"]


def _managed_env(monkeypatch, *, in_tmux=True, pane="%9", daemon_ok=True,
                 minted="tid-minted", forked="tid-forked"):
    state = {"minted": [], "forked": [], "records": [], "cleared": [], "trusted": []}
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: in_tmux)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: pane)
    if pane:
        monkeypatch.setenv("TMUX_PANE", pane)
    else:
        monkeypatch.delenv("TMUX_PANE", raising=False)
    cas = "hive.adapters.codex_app_server"
    monkeypatch.setattr(f"{cas}.spawn_daemon", lambda **_kw: daemon_ok)
    monkeypatch.setattr(f"{cas}.shared_socket_path", lambda: Path(_SHARED_SOCK))
    monkeypatch.setattr(
        f"{cas}.ensure_dir_trusted", lambda cwd: state["trusted"].append(cwd)
    )
    monkeypatch.setattr(
        f"{cas}.start_member_thread",
        lambda cwd, *, name, model="": (
            state["minted"].append((cwd, name, model)) or minted
        ),
    )
    monkeypatch.setattr(
        f"{cas}.fork_member_thread",
        lambda tid, *, name: state["forked"].append((tid, name)) or forked,
    )
    monkeypatch.setattr(
        f"{cas}.write_pane_thread",
        lambda p, tid, cwd: state["records"].append((p, tid, cwd)),
    )
    monkeypatch.setattr(
        f"{cas}.clear_pane_thread", lambda p: state["cleared"].append(p)
    )
    return state


def test_codex_bare_launch_mints_and_resumes_recorded_thread(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex"])
    assert calls == [[*_INJECTED, "--cd", os.getcwd(), "resume", "tid-minted"]]
    assert state["minted"] == [(os.getcwd(), "hive-9", "")]
    assert state["records"] == [("%9", "tid-minted", os.getcwd())]
    assert state["trusted"] == [os.getcwd()]


def test_codex_prompt_rides_resume_positional(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "fix the bug"])
    argv = calls[0]
    assert "--remote" in argv and f"unix://{_SHARED_SOCK}" in argv
    assert "check_for_update_on_startup=false" in argv
    assert argv[-3:] == ["resume", "tid-minted", "fix the bug"]
    assert state["records"] == [("%9", "tid-minted", os.getcwd())]


def test_codex_resume_with_id_records_the_pane_thread(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "resume", "sess-123"])
    assert calls[0][-2:] == ["resume", "sess-123"]
    assert "--remote" in calls[0]
    assert state["records"] == [("%9", "sess-123", os.getcwd())]
    assert state["minted"] == []  # an explicit resume mints nothing


@pytest.mark.parametrize("args", [["resume"], ["resume", "--last"]])
def test_codex_resume_picker_clears_stale_record(runner, monkeypatch, args):
    # The picked thread is unknowable up front: a stale record must not keep
    # routing hive at the pane's previous thread.
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", *args])
    assert "--remote" in calls[0]
    assert state["records"] == []
    assert state["cleared"] == ["%9"]


def test_codex_passthrough_subcommand_runs_raw(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "exec", "echo", "hi"])
    assert calls == [["codex", "codex", "exec", "echo", "hi"]]


@pytest.mark.parametrize("flag", ["-h", "--help", "-V", "--version"])
def test_codex_noninteractive_flags_run_raw(runner, monkeypatch, flag):
    # --help/--version never start a session: no daemon, codex's own output
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.spawn_daemon",
        lambda **_kw: pytest.fail(f"spawn_daemon must not run for {flag}"),
    )
    runner.invoke(cli, ["codex", flag])
    assert calls == [["codex", "codex", flag]]


def test_codex_fork_is_forked_server_side_and_resumed(runner, monkeypatch):
    # `fork <sid>` is intercepted: hive forks server-side, records the fork
    # as the pane's thread, and the TUI attaches with a plain resume.
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "-c", "k=v", "fork", "sid-1"])
    assert calls == [[
        *_INJECTED, "--cd", os.getcwd(),
        "-c", "k=v", "resume", "tid-forked",
    ]]
    assert state["forked"] == [("sid-1", "hive-9")]
    assert state["records"] == [("%9", "tid-forked", os.getcwd())]


def test_codex_fork_rpc_failure_falls_back_unrecorded(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch, forked=None)
    runner.invoke(cli, ["codex", "fork", "sid-1"])
    # codex forks on its own, remote-attached; the pane record is cleared so
    # hive never routes at a thread this TUI is not running.
    assert calls == [[*_INJECTED, "--cd", os.getcwd(), "fork", "sid-1"]]
    assert state["records"] == []
    assert state["cleared"] == ["%9"]


def test_codex_passthrough_subcommand_after_global_options(runner, monkeypatch):
    # codex allows global options before the subcommand; -c consumes its value,
    # so `exec` must still be detected as a management subcommand -> raw.
    calls = _capture_exec(monkeypatch)
    _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "-c", "foo=1", "exec", "echo", "hi"])
    assert calls == [["codex", "codex", "-c", "foo=1", "exec", "echo", "hi"]]


def test_codex_model_flag_is_interactive_launch_and_pins_mint(runner, monkeypatch):
    # `-m gpt5` is a value-taking option with no subcommand -> managed launch,
    # and the model pins the minted thread.
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "-m", "gpt5"])
    argv = calls[0]
    assert "--remote" in argv and argv[-4:] == ["resume", "tid-minted", "-m", "gpt5"]
    assert state["minted"] == [(os.getcwd(), "hive-9", "gpt5")]


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
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "-C", "/work"])
    argv = calls[0]
    assert argv.count("--cd") == 0
    assert argv[-2:] == ["-C", "/work"]
    assert "--remote" in argv
    # the user's cwd is what gets trusted and recorded
    assert state["trusted"] == ["/work"]
    assert state["records"] == [("%9", "tid-minted", "/work")]


def test_codex_does_not_double_cwd_equals_form(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch)
    runner.invoke(cli, ["codex", "--cd=/work"])
    argv = calls[0]
    assert argv.count("--cd") == 0
    assert "--cd=/work" in argv
    assert "--remote" in argv
    assert state["trusted"] == ["/work"]


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


def test_codex_mint_failure_attaches_unrecorded(runner, monkeypatch):
    calls = _capture_exec(monkeypatch)
    state = _managed_env(monkeypatch, minted=None)
    runner.invoke(cli, ["codex"])
    # degraded but alive: remote attach with no resume injection, stale
    # record cleared so hive does not route at a thread this TUI won't run
    assert calls == [[*_INJECTED, "--cd", os.getcwd()]]
    assert state["records"] == []
    assert state["cleared"] == ["%9"]


def test_shell_init_zsh_emits_hcodex_launcher(runner):
    out = runner.invoke(cli, ["shell-init", "zsh"]).output
    # function form: immune to alias expansion of the name (user aliases)
    assert "function hcodex {" in out
    assert 'if hive codex "$@"; then _hive_rc=0; else _hive_rc=$?; fi' in out
    assert "command -v hive" in out
    # plain `codex` keeps its own binary: no wrapper is defined for the bare name
    assert re.search(r"^function codex\b", out, re.M) is None


def test_shell_init_fish_emits_hcodex_launcher(runner):
    out = runner.invoke(cli, ["shell-init", "fish"]).output
    assert "function hcodex" in out
    assert "hive codex $argv" in out
    assert "if not type -q hive" in out
    assert re.search(r"^function codex\b", out, re.M) is None


# --- init gate: an unmanaged codex must relaunch hive-managed ---

import hive.cli as cli_mod


def _profile(name):
    return type("P", (), {"name": name})()


def _isolate_codex_home(monkeypatch, tmp_path):
    """Point CODEX_HOME at an empty dir so no host thread records leak in."""
    monkeypatch.setenv("CODEX_HOME", str(tmp_path / "codex-home"))


def test_init_gate_requires_a_claude_job_record(monkeypatch):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("claude")
    )
    monkeypatch.setattr("hive.adapters.claude_bg.job_id_for_pane", lambda _p: None)
    with pytest.raises(SystemExit):
        cli_mod._require_daemon_backed("%1")


def test_init_gate_allows_a_job_backed_claude(monkeypatch):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("claude")
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.job_id_for_pane", lambda _p: "cafe1234"
    )
    cli_mod._require_daemon_backed("%1")  # recorded job -> hive-managed, fine


def test_init_gate_allows_recorded_thread_on_live_daemon(monkeypatch):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("codex")
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.thread_id_for_pane", lambda _p: "tid-1"
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.daemon_alive", lambda: True)
    cli_mod._require_daemon_backed("%1")  # recorded + daemon answers -> allowed


def test_init_gate_allows_codex_tool_env_with_resolvable_thread(monkeypatch):
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-9")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _p: (_ for _ in ()).throw(
            AssertionError("profile should not be used")
        ),
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_for_thread",
        lambda tid: "%9" if tid == "thread-9" else None,
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.daemon_alive", lambda: True)
    cli_mod._require_daemon_backed("%bad")  # thread record wins over the pane arg


def test_init_gate_blocks_codex_tool_without_thread_record(monkeypatch, tmp_path, capsys):
    _isolate_codex_home(monkeypatch, tmp_path)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-1")
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane",
        lambda _p: (_ for _ in ()).throw(
            AssertionError("profile should not be used")
        ),
    )
    with pytest.raises(SystemExit):
        cli_mod._require_daemon_backed("%wrong")
    err = capsys.readouterr().err
    assert "hive codex resume" in err


def test_init_gate_blocks_unmanaged_codex_with_relaunch_steps(monkeypatch, capsys):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("codex")
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.thread_id_for_pane", lambda _p: None
    )
    with pytest.raises(SystemExit):
        cli_mod._require_daemon_backed("%1")
    err = capsys.readouterr().err
    assert "Ctrl-C" in err
    assert "hive codex resume" in err


def test_init_gate_blocks_recorded_pane_when_daemon_is_down(monkeypatch, capsys):
    monkeypatch.setattr(
        "hive.cli.detect_profile_for_pane", lambda _p: _profile("codex")
    )
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.thread_id_for_pane", lambda _p: "tid-1"
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.daemon_alive", lambda: False)
    with pytest.raises(SystemExit):
        cli_mod._require_daemon_backed("%1")
    assert "hive codex resume" in capsys.readouterr().err


def test_codex_degraded_gate_fails_every_time(monkeypatch, tmp_path, capsys):
    _isolate_codex_home(monkeypatch, tmp_path)
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


def test_codex_degraded_gate_fails_user_commands_from_cli(runner, monkeypatch, tmp_path):
    _isolate_codex_home(monkeypatch, tmp_path)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-2")

    team_result = runner.invoke(cli, ["team"])
    send_result = runner.invoke(cli, ["send", "orch", "hi"])

    assert team_result.exit_code == 1
    assert send_result.exit_code == 1
    assert "hive codex resume" in team_result.stderr
    assert "hive codex resume" in send_result.stderr


def test_codex_degraded_gate_skips_bypass_and_recorded_thread(monkeypatch, tmp_path, capsys):
    _isolate_codex_home(monkeypatch, tmp_path)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-2")
    cli_mod._require_codex_native("wait-status")
    assert capsys.readouterr().err == ""

    # a resolvable thread record makes this codex native — no gate
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.pane_for_thread",
        lambda tid: "%9" if tid == "thread-2" else None,
    )
    cli_mod._require_codex_native("team")
    assert capsys.readouterr().err == ""


def test_codex_degraded_bypass_command_reaches_own_error(runner, monkeypatch, tmp_path):
    _isolate_codex_home(monkeypatch, tmp_path)
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
    """Stub hive/codex that log calls: the managed launch (stub hive) exits
    with codex's own status 7 and must not trigger any second launch. The
    plain `codex` stub is the negative control — the launcher never runs it."""
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text(f'#!/bin/sh\necho "codex $@" >> {log}\nexit 7\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "codex" ] && exit 7\nexit 0\n'
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
         'eval "$HIVE_SHELL_INIT"; hcodex hello; echo "rc=$?"; true'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    # the launcher IS the codex run (exits 7); the hint follows with no
    # second launch in between
    assert lines == ["hive codex hello", "hive resume-hint codex"]
    assert "rc=7" in r.stdout  # the hint call must not eat codex's exit code


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
def test_shell_init_fish_codex_hint_runs_after_codex_and_keeps_exit_code(runner, tmp_path):
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    log, bin_dir = _codex_hint_stub_bins(tmp_path)
    r = subprocess.run(
        ["fish", "-c",
         f'source {shlex.quote(str(script_file))}; '
         'hcodex hello; echo "rc=$status"; true'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines == ["hive codex hello", "hive resume-hint codex"]
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


@pytest.mark.parametrize("shell", ["zsh", "bash"])
@pytest.mark.parametrize("own_rc", [7, 127])
def test_shell_init_posix_no_relaunch_on_codex_own_exit_code(runner, tmp_path, shell, own_rc):
    # regression: `hive codex || command codex` used to start a SECOND raw
    # codex whenever the exec'd codex exited nonzero on its own
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text(f'#!/bin/sh\necho "codex $@" >> {log}\nexit 0\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "codex" ] && exit {own_rc}\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    r = subprocess.run(
        [shell, "-c", 'eval "$HIVE_SHELL_INIT"; hcodex hello; echo "rc=$?"'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive codex hello"  # codex's own status
    assert lines[1] == "hive resume-hint codex"  # no raw relaunch in between
    assert len(lines) == 2
    assert f"rc={own_rc}" in r.stdout


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
@pytest.mark.parametrize("own_rc", [7, 127])
def test_shell_init_fish_no_relaunch_on_codex_own_exit_code(runner, tmp_path, own_rc):
    # 127 discriminates against any exit-code-sentinel fallback
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text(f'#!/bin/sh\necho "codex $@" >> {log}\nexit 0\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "codex" ] && exit {own_rc}\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    r = subprocess.run(
        ["fish", "-c",
         f'source {shlex.quote(str(script_file))}; hcodex hello; echo "rc=$status"'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive codex hello"
    assert lines[1] == "hive resume-hint codex"  # no raw relaunch in between
    assert len(lines) == 2
    assert f"rc={own_rc}" in r.stdout


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_posix_hcodex_without_hive_on_path_returns_127(runner, tmp_path, shell):
    # the launcher is hive's entry point, not a codex wrapper: with hive gone
    # it must say so and fail, never silently start a plain codex
    bash = shutil.which(shell)
    if bash is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log, bin_dir = _codex_hint_stub_bins(tmp_path)
    (bin_dir / "hive").unlink()
    r = subprocess.run(
        [bash, "-c", 'eval "$HIVE_SHELL_INIT"; hcodex hello; echo "rc=$?"'],
        env={"HIVE_SHELL_INIT": script, "PATH": str(bin_dir)},
        capture_output=True, text=True, timeout=15)
    assert "rc=127" in r.stdout
    assert "hcodex: hive is not on PATH" in r.stderr
    assert not log.exists()  # no fallback launch of the plain codex binary
