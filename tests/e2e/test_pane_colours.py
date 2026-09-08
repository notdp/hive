"""Real hived colour reports with a delayed terminal theme response.

The wrapper pins every tmux call, including hived's, to a named disposable
server. No engine CLI, real terminal, or user configuration participates.
"""

import fcntl
import json
import os
import pty
import re
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import uuid
from pathlib import Path

import pytest

from tests.e2e._helpers import hive_binary_argv, wait_for


class Terminal:
    """Drain an attached tmux client and answer OSC queries, withholding 997."""

    def __init__(self, env: dict[str, str], team: str):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.child = subprocess.Popen(
            ["tmux", "-u", "attach", "-t", team],
            env={**env, "TERM": "xterm-256color"},
            stdin=slave, stdout=slave, stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        self.stopped = threading.Event()
        self.theme_requested = threading.Event()
        self.thread = threading.Thread(target=self.drain, daemon=True)
        self.thread.start()

    def drain(self):
        pending = b""
        query = re.compile(rb"\x1b\](10|11);\?(?:\x07|\x1b\\)")
        while not self.stopped.is_set():
            if not select.select([self.master], [], [], 0.1)[0]:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            pending += chunk
            if b"\x1b[?996n" in pending:
                self.theme_requested.set()
            matches = list(query.finditer(pending))
            for match in matches:
                colour = b"ffff/ffff/ffff" if match[1] == b"10" else b"0000/0000/0000"
                os.write(self.master, b"\x1b]" + match[1] + b";rgb:" + colour + b"\x1b\\")
            if matches:
                pending = pending[matches[-1].end():]
            pending = pending[-64:]

    def report_dark(self):
        os.write(self.master, b"\x1b[?997;1n")

    def close(self):
        self.child.terminate()
        try:
            self.child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait(timeout=5)
        self.stopped.set()
        self.thread.join(timeout=2)
        os.close(self.master)


PROBE = r'''
import json, os, select, sys, termios, time, tty
from pathlib import Path
root = Path(sys.argv[1])
fd = os.open('/dev/tty', os.O_RDWR)
saved = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    for step in range(int(sys.argv[2])):
        if step:
            deadline = time.monotonic() + 20
            while not (root / 'again').exists():
                assert time.monotonic() < deadline, 'no second query signal'
                time.sleep(0.02)
        os.write(fd, b'\x1b]11;?\x1b\\')
        reply = b''
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if select.select([fd], [], [], 0.1)[0]:
                reply += os.read(fd, 4096)
                if b'\x1b\\' in reply or b'\x07' in reply:
                    break
        (root / ('reply-%d.json' % step)).write_text(json.dumps(reply.decode('ascii')))
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, saved)
    os.close(fd)
'''


@pytest.mark.skipif(shutil.which("tmux") is None, reason="tmux is required for e2e tests")
@pytest.mark.parametrize("probe_before_theme", [False, True], ids=["attach-before-probe", "probe-before-997"])
def test_e2e_hived_follows_delayed_client_theme(probe_before_theme):
    real_tmux = shutil.which("tmux")
    version = subprocess.check_output([real_tmux, "-V"], text=True).strip()
    parsed = re.search(r"(\d+)\.(\d+)", version)
    if not parsed or tuple(map(int, parsed.groups())) < (3, 6):
        pytest.skip("client_theme requires tmux 3.6+")
    server = f"probe-{uuid.uuid4().hex[:12]}"
    with tempfile.TemporaryDirectory(prefix="hive-colour-", dir="/tmp") as directory:
        root = Path(directory)
        bindir = root / "bin"
        bindir.mkdir()
        trace = root / "tmux.jsonl"
        config = root / "tmux.conf"
        config.write_text("set -g default-shell /bin/sh\nset -g default-command /bin/sh\nset -g window-size manual\n")
        wrapper = bindir / "tmux"
        wrapper.write_text(
            f"#!{sys.executable}\nimport json, os, sys\n"
            f"with open({str(trace)!r}, 'a') as log: log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            f"os.execv({real_tmux!r}, [{real_tmux!r}, '-L', {server!r}, '-f', {str(config)!r}, *sys.argv[1:]])\n"
        )
        wrapper.chmod(0o755)
        env = {**os.environ, "PATH": f"{bindir}:{os.environ['PATH']}",
               "HOME": str(root), "HIVE_HOME": str(root / "hive"),
               "CLAUDE_HOME": str(root / "claude"), "CLAUDE_CONFIG_DIR": str(root / "claude"),
               "CODEX_HOME": str(root / "codex"), "GROK_HOME": str(root / "grok"),
               "XDG_CACHE_HOME": str(root / "cache"), "TERM": "xterm-256color"}
        for key in ("TMUX", "TMUX_PANE", "TMUX_TMPDIR", "CODEX_THREAD_ID", "GROK_SESSION_ID",
                    "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_HOST_SESSION_ID",
                    "HIVE_VIEW_THEME", "HIVE_APPEARANCE", "COLORFGBG"):
            env.pop(key, None)
        workspace = root / "ws"
        team = "colour-test"
        human = None
        daemon = None

        def tmux(*args):
            return subprocess.check_output([str(wrapper), "-u", *args], env=env, text=True, timeout=10).strip()

        def selections():
            path = workspace / "run" / "notify.jsonl"
            if not path.exists():
                return []
            return [event for line in path.read_text().splitlines()
                    if (event := json.loads(line)).get("event") == "pane-colours.selected"]

        def start_probe(queries):
            probe = root / "probe.py"
            probe.write_text(PROBE)
            command = shlex.join([sys.executable, str(probe), str(root), str(queries)])
            tmux("respawn-pane", "-k", "-t", pane, command)

        def reply(step):
            path = root / f"reply-{step}.json"
            wait_for(path.exists, timeout=10)
            return json.loads(path.read_text())

        def pane_enumerations():
            return sum("list-panes" in args and any(a.startswith("C\t") for a in args)
                       for line in trace.read_text().splitlines() if (args := json.loads(line)))

        try:
            created = subprocess.run([*hive_binary_argv(), "create", team, "--workspace", str(workspace)],
                                     env=env, cwd=root, text=True, capture_output=True, timeout=30)
            assert created.returncode == 0, created.stderr
            window = tmux("list-windows", "-t", team, "-F", "#{window_id}").splitlines()[0]
            pane = tmux("list-panes", "-t", window, "-F", "#{pane_id}").splitlines()[0]
            with (root / "daemon.stderr").open("w") as stderr:
                daemon = subprocess.Popen([*hive_binary_argv(), "--hived", str(workspace), team, team, window],
                                          env=env, cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                          stderr=stderr, start_new_session=True)
            wait_for(lambda: any(s["source"] == "fallback" for s in selections()), timeout=15)
            clients = tmux("list-clients", "-t", team, "-F", "#{client_control_mode}")
            assert clients == "1", clients
            if probe_before_theme:
                start_probe(2)
                first = reply(0)
                assert "rgb:ffff/ffff/ffff" in first, repr(first)
            human = Terminal(env, team)
            assert human.theme_requested.wait(5), "tmux did not request terminal theme"
            wait_for(lambda: "0\t\t" in tmux("list-clients", "-t", team, "-F",
                                            "#{client_control_mode}\t#{client_theme}\t#{client_name}"), timeout=5)
            # Let attach processing settle while theme is still unknown. The
            # subsequent 997 alone must be enough, with no new pane/layout.
            time.sleep(2.5)
            before = pane_enumerations()
            human.report_dark()
            wait_for(lambda: any(s["source"] == "client" and s["appearance"] == "dark" for s in selections()), timeout=8)
            assert pane_enumerations() == before, "theme update relied on pane enumeration"
            # Selection logging precedes the control connection's queued writes.
            time.sleep(0.1)
            if probe_before_theme:
                (root / "again").touch()
                dark = reply(1)
            else:
                start_probe(1)
                dark = reply(0)
            assert "rgb:0000/0000/0000" in dark, repr(dark)
            print(json.dumps({"server": server, "version": version,
                              "scenario": "probe-before-997" if probe_before_theme else "attach-before-probe",
                              "first": first if probe_before_theme else dark, "after_997": dark,
                              "pane_enumerations_during_997": pane_enumerations() - before,
                              "selections": selections()}))
        finally:
            if human:
                human.close()
            if daemon:
                daemon.terminate()
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait(timeout=5)
            subprocess.run([real_tmux, "-L", server, "kill-server"], capture_output=True, timeout=10)
            gone = subprocess.run([real_tmux, "-L", server, "list-sessions"], capture_output=True, timeout=10)
            assert gone.returncode != 0, f"private server {server} survived teardown"
            print(f"{server}: killed; list-sessions exit={gone.returncode}")
