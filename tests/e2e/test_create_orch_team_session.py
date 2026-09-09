"""Managed orch pane placement on a private tmux server; no LLM required."""

import json
import os
import pty
import shlex
import shutil
import signal
import stat
import subprocess
import tempfile
import threading
from contextlib import contextmanager
from pathlib import Path

import pytest

from tests.e2e._helpers import (
    base_env, hive_binary_argv, kill_private_server, private_socket,
    run_tmux, send_tmux_command, wait_for,
)
from tests.e2e.test_spawn_send_kill_with_a_stub_cli import (
    STUB, _pid_alive, _stub_jobs, _stub_json, _stub_pids,
)

pytestmark = pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required")


class Orch:
    def __init__(self, root):
        self.root = root
        self.config = root / "claude"
        self.team = "orch-ui"
        self.source = "human"
        bindir = root / "bin"
        bindir.mkdir()
        shutil.copy(STUB, bindir / "claude")
        (bindir / "claude").chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR)
        os.symlink(hive_binary_argv()[0], bindir / "hive")
        self.env = {
            **os.environ, **base_env(root),
            "HOME": str(root / "home"), "CLAUDE_HOME": str(self.config),
            "CLAUDE_CONFIG_DIR": str(self.config), "GROK_HOME": str(root / "grok"),
            "PATH": f"{bindir}:{os.environ['PATH']}", "SHELL": "/bin/sh", "TERM": "xterm-256color",
            **{key: "" for key in (
                "TMUX", "TMUX_PANE", "CODEX_THREAD_ID", "GROK_SESSION_ID",
                "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_HOST_SESSION_ID",
            )},
        }
        Path(self.env["HOME"]).mkdir()
        self.pane = self.tmux("new-session", "-d", "-s", self.source,
                              "-x", "120", "-y", "40", "-c", str(root),
                              "-P", "-F", "#{pane_id}", "/bin/sh")
        self.window = self.value(self.pane, "#{window_id}")
        self.tmux("set-option", "-g", "default-shell", "/bin/sh")
        self.tmux("set-option", "-g", "default-command", "/bin/sh")
        self.tmux("set-option", "-t", self.source, "status", "on")
        self.tmux("set-option", "-t", self.source, "status-right", "human-original")
        self.tmux("set-option", "-t", self.source, "mouse", "off")
        send_tmux_command(self.pane, shlex.join([hive_binary_argv()[0], "claude"]), env=self.env)
        wait_for(lambda: len(_stub_jobs(self.config)) == 1, timeout=30)
        self.job = _stub_jobs(self.config)[0]["id"]
        wait_for(lambda: _stub_json(self.config, f"engine-{self.job}.json") is not None, timeout=30)
        self.engine = _stub_json(self.config, f"engine-{self.job}.json")
        self.tool_env = {**self.env, "CLAUDE_CODE_MESSAGING_SOCKET": self.engine["socket"]}

        def bound():
            result = self.hive("team")
            return result.returncode == 0 and json.loads(result.stdout).get("tmux", {}).get("currentPane") == self.pane

        wait_for(bound, timeout=30)
        self.pane_pid = self.value(self.pane, "#{pane_pid}")

    def tmux(self, *args):
        return run_tmux(list(args), env=self.env).stdout.strip()

    def value(self, target, fmt):
        return self.tmux("display-message", "-p", "-t", target, fmt)

    def hive(self, *args, engine=True):
        return subprocess.run([*hive_binary_argv(), *args], env=self.tool_env if engine else self.env,
                              cwd=self.root, text=True, capture_output=True, timeout=60)

    def create(self):
        result = self.hive("create", self.team)
        assert result.returncode == 0, (result.stdout, result.stderr)
        return json.loads(result.stdout)

    def check_source(self):
        assert self.tmux("has-session", "-t", f"={self.source}") == ""
        assert self.value(self.window, "#{window_id}") == self.window
        assert self.tmux("show-options", "-t", self.source, "-v", "status") == "on"
        assert self.tmux("show-options", "-t", self.source, "-v", "status-right") == "human-original"
        assert self.tmux("show-options", "-t", self.source, "-v", "mouse") == "off"

    def check_team(self, payload):
        assert payload["orch"]["pane"] == self.pane
        assert self.value(self.pane, "#{session_name}") == self.team
        assert self.value(self.pane, "#{pane_pid}") == self.pane_pid
        assert self.tmux("show-options", "-t", self.team, "-v", "status") == "2"
        assert self.value(self.pane, "#{@hive-agent}") == "orch"
        assert self.value(self.pane, "#{@hive-built}") == "1"
        entry = json.loads((Path(payload["workspace"]) / "team.json").read_text())
        assert entry["members"][0]["sessionId"] == self.job
        assert len(_stub_jobs(self.config)) == 1
        self.check_source()


@pytest.fixture
def orch():
    root = Path(tempfile.mkdtemp(prefix="hui-", dir="/tmp"))
    rig = None
    try:
        rig = Orch(root)
        yield rig
    finally:
        if rig:
            rig.hive("delete", rig.team, "--down", engine=False)
        config = root / "claude"
        for pid in set(_stub_pids(config) + [j.get("pid") for j in _stub_jobs(config)]):
            if pid and pid != os.getpid() and _pid_alive(pid):
                os.kill(pid, signal.SIGKILL)
        kill_private_server(base_env(root))
        shutil.rmtree(root)


def test_e2e_single_pane_create_keeps_source_and_same_managed_pane(orch):
    payload = orch.create()
    orch.check_team(payload)
    assert "hive attach orch-ui" in payload["nextStep"]
    replacement, = orch.tmux("list-panes", "-t", orch.window, "-F", "#{pane_id}").splitlines()
    assert replacement != orch.pane
    assert Path(orch.value(replacement, "#{pane_current_path}")).resolve() == orch.root.resolve()
    repeated = orch.create()
    assert repeated["team"] == orch.team
    assert orch.value(orch.pane, "#{pane_pid}") == orch.pane_pid
    removed = orch.hive("delete", orch.team, "--down", engine=False)
    assert removed.returncode == 0, removed.stderr
    orch.check_source()


def test_e2e_multi_pane_create_keeps_other_source_panes(orch):
    other = orch.tmux("split-window", "-d", "-t", orch.window, "-P", "-F", "#{pane_id}", "/bin/sh")
    other_pid = orch.value(other, "#{pane_pid}")
    payload = orch.create()
    orch.check_team(payload)
    assert orch.tmux("list-panes", "-t", orch.window, "-F", "#{pane_id}").splitlines() == [other]
    assert orch.value(other, "#{pane_pid}") == other_pid


def test_e2e_same_name_user_session_refused_without_moving(orch):
    orch.team = orch.source
    result = orch.hive("create", orch.team)
    assert result.returncode != 0
    assert "not owned by Hive" in result.stderr
    assert orch.value(orch.pane, "#{window_id}") == orch.window
    assert orch.value(orch.pane, "#{pane_pid}") == orch.pane_pid
    assert not (Path(orch.env["HIVE_HOME"]) / "teams" / orch.team / "team.json").exists()
    orch.check_source()
    refused = orch.hive("delete", orch.team, "--down", engine=False)
    assert refused.returncode != 0
    orch.check_source()


def test_e2e_linked_source_refused_without_moving(orch):
    orch.tmux("new-session", "-d", "-s", "linked", "/bin/sh")
    orch.tmux("link-window", "-s", orch.window, "-t", "linked:")
    result = orch.hive("create", orch.team)
    assert result.returncode != 0
    assert "linked across sessions" in result.stderr
    assert orch.value(orch.pane, "#{window_id}") == orch.window
    orch.check_source()


@contextmanager
def terminal_client(orch):
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe("tmux", ["tmux", "-S", private_socket(orch.env), "attach-session", "-t", orch.source], orch.env)
    def drain():
        try:
            while os.read(fd, 65536):
                pass
        except OSError:
            pass
    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    try:
        def attached():
            return str(pid) in orch.tmux("list-clients", "-F", "#{client_pid}").splitlines()
        wait_for(attached, timeout=10)
        yield pid
    finally:
        try:
            os.kill(pid, signal.SIGHUP)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
        reader.join(timeout=2)
        os.close(fd)


def test_e2e_create_switches_the_attached_terminal_client(orch):
    with terminal_client(orch) as pid:
        payload = orch.create()
        orch.check_team(payload)
        assert "nextStep" not in payload
        rows = orch.tmux("list-clients", "-F", "#{client_pid}\t#{session_name}\t#{window_id}").splitlines()
        assert f"{pid}\t{orch.team}\t{orch.value(orch.pane, '#{window_id}')}" in rows
