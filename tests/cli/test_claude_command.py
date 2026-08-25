"""CLI tests for the `hive claude` managed launcher and its shell-init function."""
import os
import shlex
import shutil
import subprocess

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


@pytest.mark.parametrize("argv", [
    [],                                    # bare launch
    ["hello"],                             # positional prompt
    ["mcp", "list"],                       # management subcommand
    ["daemon", "status"],                  # subcommand hidden from `claude --help`
    ["-p", "say hi"],                      # non-interactive print run
    ["--model", "x", "-r", "sid", "hi"],   # value-taking global options
])
def test_claude_outside_tmux_execs_claude_with_argv_verbatim(runner, monkeypatch, argv):
    # outside tmux there is no pane to bind a job to: every argv shape
    # reaches plain claude untouched.
    monkeypatch.delenv("TMUX", raising=False)
    calls = _capture_exec(monkeypatch)
    runner.invoke(cli, ["claude", *argv])
    assert calls == [["claude", "claude", *argv]]


@pytest.mark.parametrize("argv", [
    ["mcp", "list"],                       # management subcommand
    ["daemon", "status"],                  # subcommand hidden from `claude --help`
    ["-p", "say hi"],                      # headless print (rejected by --bg)
    ["--bg", "task"],                      # caller manages the job itself
    ["-c"],                                # continue: session unknowable up front
    ["-r"],                                # resume picker: ditto
])
def test_claude_non_interactive_shapes_go_raw_even_in_tmux(runner, monkeypatch, argv):
    _managed_env(monkeypatch)
    calls = _capture_exec(monkeypatch)
    runner.invoke(cli, ["claude", *argv])
    assert calls == [["claude", "claude", *argv]]


def test_claude_help_is_forwarded_not_handled_by_click(runner, monkeypatch):
    # add_help_option=False: `hclaude --help` must show claude's help, not
    # hive's, so the flag has to survive click and reach the exec
    calls = _capture_exec(monkeypatch)
    result = runner.invoke(cli, ["claude", "--help"])
    assert calls == [["claude", "claude", "--help"]]
    assert "Usage: cli claude" not in result.output


# --- managed launch: bg job + attach loop ------------------------------------


def _managed_env(monkeypatch):
    monkeypatch.setenv("TMUX", "/tmp/tmux-test/default,1,0")
    monkeypatch.setenv("TMUX_PANE", "%7")


def _fake_engine(job_id="cafe1234", session_id="sess-1"):
    from hive.adapters.claude_bg import EngineSession

    return EngineSession(
        pid=4242, job_id=job_id, session_id=session_id,
        socket_path="/tmp/cc-socks/4242.sock", cwd="/tmp",
        status="idle", waiting_for="", status_updated_at=0.0,
    )


def _mock_bg(monkeypatch, *, job_id="cafe1234"):
    state = {"spawns": [], "records": [], "loops": [], "wakes": []}
    monkeypatch.setattr(
        "hive.adapters.claude_bg.spawn_job",
        lambda **kw: state["spawns"].append(kw) or job_id,
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.wait_engine_entry",
        lambda _jid, timeout=0: _fake_engine(job_id=job_id),
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.engine_session_for_job", lambda _jid: None
    )
    monkeypatch.setattr("hive.adapters.claude_bg.job_exists", lambda _jid, **_kw: False)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.ensure_engine",
        lambda jid, **_kw: state["wakes"].append(jid) or _fake_engine(job_id=jid),
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.write_pane_job",
        lambda pane, jid, sid, cwd: state["records"].append((pane, jid, sid, cwd)),
    )
    monkeypatch.setattr(
        "hive.cli._claude_attach_loop", lambda jid: state["loops"].append(jid) or 0
    )
    return state


def test_claude_interactive_launch_mints_bg_job_and_attaches(runner, monkeypatch):
    _managed_env(monkeypatch)
    calls = _capture_exec(monkeypatch)
    state = _mock_bg(monkeypatch)

    result = runner.invoke(cli, ["claude", "--model", "opus", "hi"])

    assert result.exit_code == 0
    assert calls == []  # no raw exec: the pane holds the attach loop
    spawn = state["spawns"][0]
    assert spawn["name"] == "hive-7"
    assert spawn["extra_args"] == ["--model", "opus", "hi"]
    assert state["records"] == [("%7", "cafe1234", "sess-1", os.getcwd())]
    assert state["loops"] == ["cafe1234"]


def test_claude_resume_of_a_known_job_attaches_without_minting(runner, monkeypatch):
    _managed_env(monkeypatch)
    calls = _capture_exec(monkeypatch)
    state = _mock_bg(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.claude_bg.engine_session_for_job",
        lambda jid: _fake_engine(job_id=jid) if jid == "cafe1234" else None,
    )

    result = runner.invoke(cli, ["claude", "--resume", "cafe1234"])

    assert result.exit_code == 0
    assert calls == []
    assert state["spawns"] == []  # rebind, not a new job
    assert state["records"] == [("%7", "cafe1234", "sess-1", os.getcwd())]
    assert state["loops"] == ["cafe1234"]


def test_claude_resume_of_a_session_uuid_mints_a_bg_resume(runner, monkeypatch):
    _managed_env(monkeypatch)
    calls = _capture_exec(monkeypatch)
    state = _mock_bg(monkeypatch)
    sid = "74e0fe8d-3278-436a-98f1-7dd32c817571"

    result = runner.invoke(cli, ["claude", "-r", sid, "--fork-session"])

    assert result.exit_code == 0
    assert calls == []
    spawn = state["spawns"][0]
    assert spawn["extra_args"] == ["-r", sid, "--fork-session"]
    assert state["loops"] == ["cafe1234"]


def test_claude_bg_spawn_failure_falls_back_to_raw(runner, monkeypatch):
    _managed_env(monkeypatch)
    calls = _capture_exec(monkeypatch)
    state = _mock_bg(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_bg.spawn_job", lambda **_kw: None)

    runner.invoke(cli, ["claude", "hello"])

    assert calls == [["claude", "claude", "hello"]]
    assert state["loops"] == []


def test_shell_init_zsh_emits_hclaude_launcher(runner):
    result = runner.invoke(cli, ["shell-init", "zsh"])
    assert result.exit_code == 0
    # ksh-style `function name {` bypasses alias expansion of the name in
    # both zsh and bash, so a stray user alias cannot break the parse
    assert "function hcodex {" in result.output
    assert "function hclaude {" in result.output
    # errexit-safe status capture; the raw fallback lives in `hive claude`
    assert 'if hive claude "$@"; then _hive_rc=0; else _hive_rc=$?; fi' in result.output
    assert "command -v hive" in result.output
    # the plain command keeps its own meaning: shell-init never redefines it
    assert "function claude {" not in result.output
    assert "function codex {" not in result.output


def test_shell_init_bash_emits_launcher_function_form(runner):
    result = runner.invoke(cli, ["shell-init", "bash"])
    assert result.exit_code == 0
    assert "function hcodex {" in result.output
    assert "function hclaude {" in result.output


def test_shell_init_fish_emits_hclaude_launcher(runner):
    result = runner.invoke(cli, ["shell-init", "fish"])
    assert result.exit_code == 0
    assert "function hcodex" in result.output
    assert "function hclaude" in result.output
    assert "hive claude $argv" in result.output
    assert "if not type -q hive" in result.output


# --- resume-hint (claude) -----------------------------------------------------


def test_shell_init_posix_claude_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "zsh"]).output
    assert 'hive resume-hint claude 2>/dev/null' in out
    assert "return $_hive_rc" in out
    # the claude hint is emitted once, in the hclaude launcher only
    assert out.count("hive resume-hint claude 2>/dev/null") == 1


def test_shell_init_fish_claude_tail_calls_resume_hint(runner):
    out = runner.invoke(cli, ["shell-init", "fish"]).output
    assert "hive resume-hint claude 2>/dev/null" in out
    assert "return $_hive_rc" in out
    assert out.count("hive resume-hint claude 2>/dev/null") == 1


def _hint_stub_bins(tmp_path):
    """Stub hive/claude that log calls: the managed launch (stub hive) exits
    with claude's own status 7 — the launcher execs, so its status IS the
    CLI's — and must not trigger any second launch."""
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\nexit 7\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "claude" ] && exit 7\nexit 0\n'
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
         'eval "$HIVE_SHELL_INIT"; hclaude hello; echo "rc=$?"; true'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"  # the launcher IS the claude run (exits 7)
    assert lines[1] == "hive resume-hint claude"  # no second launch in between
    assert len(lines) == 2
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
         'hclaude hello; echo "rc=$status"; true'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
             "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"
    assert lines[1] == "hive resume-hint claude"
    assert len(lines) == 2
    assert "rc=7" in r.stdout

def _member_pane_env(monkeypatch, tmp_path, *, pane="%5", team="t1", agent="worker"):
    monkeypatch.setenv("HIVE_HOME", str(tmp_path / "hive-home"))
    monkeypatch.setenv("TMUX_PANE", pane)
    tags = {"hive-team": team, "hive-agent": agent}
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option", lambda _p, key: tags.get(key)
    )
    return tmp_path / "hive-home"


def test_resume_hint_reads_pane_job_record_and_quotes(runner, monkeypatch, tmp_path):
    # the launch records the pane's bg jobId, and the record outlives viewer
    # and engine alike — the hint reads that, never the process table; the
    # cwd is still quoted
    work = tmp_path / "wo rk"
    work.mkdir()
    monkeypatch.chdir(work)
    _member_pane_env(monkeypatch, tmp_path)
    from hive.adapters import claude_bg

    claude_bg.write_pane_job("%5", "cafe1234", "sid-1", str(work))
    cwd = os.getcwd()
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == (
        "Resume from anywhere:\n"
        f"  cd {shlex.quote(cwd)} && hive claude --resume cafe1234\n"
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


def test_resume_hint_missing_job_record_prints_nothing(runner, monkeypatch, tmp_path):
    _member_pane_env(monkeypatch, tmp_path)
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == ""


@pytest.mark.parametrize("evil_id", [
    "ok\x1b]52;c;AAAA\x07",   # OSC 52 clipboard write, ESC + BEL
    "ok\nrm -rf ~",           # newline splits the printed command line
    "--dangerously-skip-permissions",  # option-shaped id
])
def test_resume_hint_record_id_untrusted_gates(runner, monkeypatch, tmp_path, evil_id):
    # record content is still untrusted for printing: quoting protects a
    # later shell parse, not the automatic print, and a leading-dash id
    # would parse as a flag (`claude --resume` takes an optional value)
    _member_pane_env(monkeypatch, tmp_path)
    from hive.adapters import claude_bg

    claude_bg.write_pane_job("%5", evil_id, "", "/tmp")
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == ""


def test_resume_hint_control_bytes_in_cwd_silence_hint(runner, monkeypatch, tmp_path):
    evil_dir = tmp_path / "d\x1b]0;pwned\x07ir"
    evil_dir.mkdir()
    _member_pane_env(monkeypatch, tmp_path)
    from hive.adapters import claude_bg

    claude_bg.write_pane_job("%5", "cafe1234", "", str(evil_dir))
    monkeypatch.chdir(evil_dir)
    result = runner.invoke(cli, ["resume-hint", "claude"])
    assert result.exit_code == 0
    assert result.output == ""


@pytest.mark.parametrize("shell", ["zsh", "bash"])
@pytest.mark.parametrize("own_rc", [7, 127])
def test_shell_init_posix_no_relaunch_on_claude_own_exit_code(runner, tmp_path, shell, own_rc):
    # regression: `hive claude || command claude` used to relaunch raw on any
    # nonzero exit of the exec'd claude — including a legitimate 127, which
    # no exit-code sentinel can distinguish from "command not found"
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\nexit 0\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "claude" ] && exit {own_rc}\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    r = subprocess.run(
        [shell, "-c", 'eval "$HIVE_SHELL_INIT"; hclaude hello; echo "rc=$?"'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"  # claude's own status
    assert lines[1] == "hive resume-hint claude"  # no raw relaunch in between
    assert len(lines) == 2
    assert f"rc={own_rc}" in r.stdout


@pytest.mark.skipif(shutil.which("fish") is None, reason="fish not available")
@pytest.mark.parametrize("own_rc", [7, 127])
def test_shell_init_fish_no_relaunch_on_claude_own_exit_code(runner, tmp_path, own_rc):
    # 127 discriminates against any exit-code-sentinel fallback
    script_file = tmp_path / "init.fish"
    script_file.write_text(runner.invoke(cli, ["shell-init", "fish"]).output)
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\nexit 0\n')
    (bin_dir / "hive").write_text(
        f'#!/bin/sh\necho "hive $@" >> {log}\n[ "$1" = "claude" ] && exit {own_rc}\nexit 0\n'
    )
    for stub in bin_dir.iterdir():
        stub.chmod(0o755)
    r = subprocess.run(
        ["fish", "-c",
         f'source {shlex.quote(str(script_file))}; hclaude hello; echo "rc=$status"'],
        env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"
    assert lines[1] == "hive resume-hint claude"
    assert len(lines) == 2
    assert f"rc={own_rc}" in r.stdout


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_posix_survives_errexit_and_keeps_status(runner, tmp_path, shell):
    # under `set -e`, a bare `hive claude "$@"` returning nonzero would kill
    # the shell before the status is saved; the if-condition capture must
    # keep the function alive through claude's own nonzero exit
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
    # bare call: a `claude ... || capture` at the call site would suppress
    # errexit inside the whole function body and prove nothing
    r = subprocess.run(
        [shell, "-c",
         'set -e; eval "$HIVE_SHELL_INIT"; hclaude hello; echo unreachable'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:{os.environ['PATH']}", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    # errexit is genuinely armed: the function's nonzero return kills the
    # outer shell with claude's own status...
    assert r.returncode == 7, r.stderr
    assert "unreachable" not in r.stdout
    # ...but INSIDE the function the status was captured and the hint still
    # ran — the old bare-command form died before either happened
    lines = log.read_text().splitlines()
    assert lines[0] == "hive claude hello"
    assert lines[1] == "hive resume-hint claude"
    assert len(lines) == 2


@pytest.mark.parametrize("shell", ["zsh", "bash"])
def test_shell_init_missing_hive_returns_127_and_reports(runner, tmp_path, shell):
    # hclaude only exists to launch a hive-connected claude: without hive it
    # says so and returns 127 instead of quietly starting an unmanaged claude
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not available")
    script = runner.invoke(cli, ["shell-init", shell]).output
    log = tmp_path / "calls.log"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "claude").write_text(f'#!/bin/sh\necho "claude $@" >> {log}\nexit 0\n')
    (bin_dir / "claude").chmod(0o755)
    r = subprocess.run(
        # absolute shell path: the child PATH below intentionally lacks the
        # dirs where real hive (and the shells) live
        [shutil.which(shell), "-c",
         'eval "$HIVE_SHELL_INIT"; hclaude hello; echo "rc=$?"'],
        env={**os.environ, "HIVE_SHELL_INIT": script,
             "PATH": f"{bin_dir}:/usr/bin:/bin", "TMUX": "stub"},
        capture_output=True, text=True, timeout=15)
    assert r.returncode == 0, r.stderr
    assert "rc=127" in r.stdout
    assert "hclaude: hive is not on PATH" in r.stderr
    assert not log.exists()  # no claude launched behind hive's back
