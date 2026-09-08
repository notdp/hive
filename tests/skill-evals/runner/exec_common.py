#!/usr/bin/env python3
"""Plumbing shared by every headless executor (claude_exec.py, codex_exec.py):
environment washing, `source env.sh` expansion, one streamed subprocess with a
deadline, markdown fences, JSON helpers. Nothing here knows an engine's event
format.
"""
import datetime
import json
import os
from pathlib import Path
import shlex
import subprocess
import threading
import time

# Environment prefixes that must not leak into a nested engine: a claude
# launched inside a Claude session refuses to run or joins the parent's
# messaging socket with CLAUDE*/CLAUDECODE set; CODEX_HOME/CODEX_THREAD_ID
# would point a codex at the user's real home or identity; HIVE_*/TMUX* would
# let the stub or the model see a real team.
WASHED_PREFIXES = ("CLAUDE", "CODEX", "HIVE_", "TMUX")


def washed_env(base=None):
    base = dict(os.environ if base is None else base)
    return {k: v for k, v in base.items() if not k.startswith(WASHED_PREFIXES)}


def washed_names(base=None):
    base = os.environ if base is None else base
    return sorted(k for k in base if k.startswith(WASHED_PREFIXES))


def shell_isolation(run, env):
    """Point the login shells an engine spawns away from the user's rc files
    and the real tmux server. `ZDOTDIR` at an empty directory under the run:
    a `zsh -l` then reads no `~/.zshenv`, which on this machine sources
    `~/.cargo/env` and prepends `~/.cargo/bin` (the real hive) ahead of the
    stub whenever the starting PATH lacks it. `TMUX_TMPDIR` at a directory
    that exists (tmux falls back to the default socket silently when it does
    not), and no `TMUX`/`TMUX_PANE`. bash has no ZDOTDIR equivalent; the
    probe below is what catches a bash rc file that reorders PATH."""
    run = Path(run)
    home = run / "home"
    tmux_tmp = run / "tmux-tmp"
    home.mkdir(exist_ok=True)
    tmux_tmp.mkdir(exist_ok=True)
    env = dict(env)
    env["ZDOTDIR"] = str(home)
    env["TMUX_TMPDIR"] = str(tmux_tmp)
    for key in ("TMUX", "TMUX_PANE"):
        env.pop(key, None)
    return env


# The login-shell forms the engines actually use for a command: codex runs
# `/bin/zsh -lc <cmd>` (the passwd shell, not $SHELL); Claude Code snapshots
# `$SHELL -l` (bash here) before its first Bash call and reuses that PATH.
PROBE_SHELLS = (("/bin/zsh", "-lc"), ("/bin/bash", "-lc"))


def probe_hive_path(run, env):
    """Isolation gate run by the runner itself before the model starts: under
    the executor's exact environment, every login-shell form must resolve
    `hive` to the run's stub, and the shell/tmux isolation variables must
    point at directories that exist. Returns the report (`ok`, `problems`)."""
    run = Path(run)
    expected = run / "bin" / "hive"
    report = {"expected": str(expected), "path": env.get("PATH"), "shells": {}, "env": {}, "problems": []}
    for key in ("ZDOTDIR", "TMUX_TMPDIR"):
        value = env.get(key)
        report["env"][key] = value
        if not value or not Path(value).is_dir():
            report["problems"].append(f"{key}={value!r} is not an existing directory")
    for key in ("TMUX", "TMUX_PANE"):
        if key in env:
            report["problems"].append(f"{key} is set in the executor environment")
    for shell, flag in PROBE_SHELLS:
        label = f"{shell} {flag}"
        try:
            proc = subprocess.run([shell, flag, "command -v hive"], cwd=str(run / "shared"), env=env,
                                  capture_output=True, text=True, timeout=30)
            entry = {"resolved": proc.stdout.strip() or None, "exit_code": proc.returncode,
                     "stderr": proc.stderr.strip()[-500:]}
        except (OSError, subprocess.TimeoutExpired) as exc:
            entry = {"resolved": None, "exit_code": None, "stderr": repr(exc)}
        report["shells"][label] = entry
        resolved = entry["resolved"]
        if not resolved or (Path(resolved) != expected and os.path.realpath(resolved) != os.path.realpath(expected)):
            report["problems"].append(f"{label} resolves hive to {resolved!r}, expected {expected}")
    report["control_layout"] = probe_control_layout(run, env)
    report["problems"].extend(report["control_layout"]["problems"])
    report["ok"] = not report["problems"]
    return report


def env_exports(script):
    """The `export K=V` lines of a prepare.py env.sh, parsed without a shell
    (shlex handles the quoting prepare writes). Enough to locate the run's
    control directory and logs when no engine environment is being built."""
    env = {}
    script = Path(script)
    if not script.is_file():
        return env
    for line in script.read_text().splitlines():
        parts = shlex.split(line, posix=True) if line.startswith("export ") else []
        for part in parts[1:]:
            if "=" in part:
                k, v = part.split("=", 1)
                env[k] = v
    return env


# Files prepare.py keeps in the hidden control directory beside the run
# (`<parent>/.run-N.control/`, exported as HIVE_EVAL_CONTROL) since the v4
# standard; an older prepare left them in the run root. `run_file` reads the
# v4 location when env.sh names it and the file is there, else the run root,
# so both layouts grade with the same code.
CONTROL_FILES = ("executor-prompt.md", "prompt.md", "hive-calls.jsonl", "host-calls.jsonl")
LOG_VARS = {"hive-calls.jsonl": "HIVE_EVAL_LOG", "host-calls.jsonl": "HIVE_EVAL_HOST_LOG"}


def control_dir(run, env=None):
    """HIVE_EVAL_CONTROL from the executor environment (or env.sh), as a Path;
    None under a prepare that has no control directory."""
    run = Path(run)
    env = env_exports(run / "env.sh") if env is None else env
    value = env.get("HIVE_EVAL_CONTROL")
    return Path(value) if value else None


def run_file(run, name, env=None):
    """Where `name` (one of CONTROL_FILES) lives for this run: the path the
    env names (HIVE_EVAL_LOG / HIVE_EVAL_HOST_LOG for the logs, the control
    directory for the prompts) when that file exists, else `<run>/<name>`."""
    run = Path(run)
    env = env_exports(run / "env.sh") if env is None else env
    named = env.get(LOG_VARS.get(name, ""))
    candidates = [Path(named)] if named else []
    control = control_dir(run, env)
    if control:
        candidates.append(control / name)
    for path in candidates:
        if path.is_file():
            return path
    return run / name


def probe_control_layout(run, env):
    """Validate the v4 paths without requiring a control directory for v3."""
    run = Path(run).resolve()
    control = control_dir(run, env)
    report = {'control_dir': str(control) if control else None, 'problems': []}
    if control is None:
        report['layout'] = 'legacy-run-root'
    else:
        control = control.resolve()
        report['layout'] = 'control-directory'
        if control == run or run in control.parents:
            report['problems'].append('control directory must be outside the executor run')
        for name in CONTROL_FILES:
            expected = control / name
            if not expected.is_file():
                report['problems'].append(f'missing control file: {expected}')
            if (run / name).exists():
                report['problems'].append(f'control file still exposed in run root: {run / name}')
        for name, key in LOG_VARS.items():
            if not env.get(key) or Path(env[key]).resolve() != control / name:
                report['problems'].append(f'{key} must name {control / name}')
    report['ok'] = not report['problems']
    return report


def env_from_script(script, base):
    """Environment after `source script` under bash, starting from base."""
    out = subprocess.run(["bash", "-c", "source " + shlex.quote(str(script)) + " && env -0"],
                         env=base, capture_output=True, check=True)
    env = {}
    for chunk in out.stdout.split(b"\0"):
        if b"=" in chunk:
            k, v = chunk.decode("utf-8", "surrogateescape").split("=", 1)
            env[k] = v
    for k in ("_", "SHLVL", "PWD", "OLDPWD"):
        env.pop(k, None)
    return env


def kill_group(proc):
    try:
        os.killpg(proc.pid, 9)
    except ProcessLookupError:
        pass
    except PermissionError:
        proc.kill()


class StreamedProcess:
    """One engine process: prompt on stdin, JSONL stdout streamed to raw_path
    line by line, stderr to a file, killed as a process group at the deadline
    or when `on_event` returns True (the caller's early stop)."""

    def __init__(self, cmd, prompt, cwd, env, raw_path, stderr_path, timeout, on_event=None):
        self.cmd, self.prompt, self.cwd, self.env = cmd, prompt, Path(cwd), env
        self.raw_path, self.stderr_path = Path(raw_path), Path(stderr_path)
        self.timeout, self.on_event = timeout, on_event
        self.events = []
        self.timed_out = False
        self.stopped_early = False
        self.exit_code = None
        self.started_at = self.finished_at = None
        self.wall_seconds = None

    def run(self):
        self.started_at = datetime.datetime.now(datetime.timezone.utc)
        t0 = time.monotonic()
        with self.stderr_path.open("wb") as err, self.raw_path.open("w", encoding="utf-8") as raw:
            proc = subprocess.Popen(self.cmd, cwd=str(self.cwd), env=self.env, stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=err, start_new_session=True)

            def feed():
                try:
                    proc.stdin.write(self.prompt.encode("utf-8"))
                    proc.stdin.close()
                except (BrokenPipeError, OSError):
                    pass

            def deadline():
                self.timed_out = True
                kill_group(proc)

            threading.Thread(target=feed, daemon=True).start()
            timer = threading.Timer(self.timeout, deadline)
            timer.start()
            try:
                for line_bytes in proc.stdout:
                    line = line_bytes.decode("utf-8", "replace")
                    raw.write(line)
                    raw.flush()
                    try:
                        ev = json.loads(line)
                    except ValueError:
                        continue
                    self.events.append(ev)
                    if self.on_event and self.on_event(ev):
                        self.stopped_early = True
                        kill_group(proc)
                        break
                proc.wait()
            finally:
                timer.cancel()
                if proc.poll() is None:
                    kill_group(proc)
                    proc.wait()
        self.exit_code = proc.returncode
        self.wall_seconds = time.monotonic() - t0
        self.finished_at = datetime.datetime.now(datetime.timezone.utc)
        return self

    def stamps(self):
        return {"started_at": self.started_at.isoformat() if self.started_at else None,
                "finished_at": self.finished_at.isoformat() if self.finished_at else None,
                "total_duration_seconds": round(self.wall_seconds, 3) if self.wall_seconds is not None else -1}


def fence(text, lang=""):
    text = "" if text is None else str(text)
    longest = 3
    run = 0
    for ch in text:
        run = run + 1 if ch == "`" else 0
        longest = max(longest, run + 1)
    fence = "`" * max(3, longest)
    return f"{fence}{lang}\n{text}\n{fence}"


def read_events(raw_path):
    events = []
    for line in Path(raw_path).read_text(encoding="utf-8").splitlines():
        try:
            events.append(json.loads(line))
        except ValueError:
            continue
    return events


def update_json(path, mutate):
    path = Path(path)
    data = json.loads(path.read_text()) if path.is_file() else {}
    mutate(data)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    return data
