"""tmux operations: pane lifecycle, send_keys, capture_pane, layout."""

from __future__ import annotations

import os
import pty
import re
import shlex
import select
import subprocess
import threading
import time
from dataclasses import dataclass


def _run(args: list[str], check: bool = True, timeout: int = 5) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(
            ["tmux", *args],
            capture_output=True, text=True, check=check, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(["tmux", *args], 1, "", "timeout")


def _run_output(args: list[str]) -> str:
    r = _run(args)
    return r.stdout.strip()


_CONTROL_MODE_OUTPUT_RE = re.compile(
    r"^%(?P<kind>extended-output|output) (?P<pane>%[0-9]+)\b"
)
_CONTROL_MODE_RESTART_DELAY = 1.0
_OUTPUT_BUFFER_MAX = 64 * 1024
_OCTAL_DIGITS = frozenset("01234567")


def _decode_output_payload(raw: str) -> str:
    """Decode tmux control-mode escape: control bytes and '\\' are encoded as \\NNN (3 octal digits)."""
    if "\\" not in raw:
        return raw
    out: list[str] = []
    i = 0
    n = len(raw)
    while i < n:
        ch = raw[i]
        if ch == "\\" and i + 3 < n and all(c in _OCTAL_DIGITS for c in raw[i + 1 : i + 4]):
            out.append(chr(int(raw[i + 1 : i + 4], 8)))
            i += 4
        else:
            out.append(ch)
            i += 1
    return "".join(out)


def parse_control_mode_output(line: str) -> tuple[str, str]:
    """Return (pane_id, decoded_payload) for a control mode output line, or ("", "")."""
    stripped = (line or "").strip()
    match = _CONTROL_MODE_OUTPUT_RE.match(stripped)
    if not match:
        return "", ""
    remainder = stripped[match.end():]
    if match.group("kind") == "extended-output":
        # format: "<age> ... : <value>"
        colon_idx = remainder.find(":")
        if colon_idx >= 0:
            remainder = remainder[colon_idx + 1 :]
    return match.group("pane"), _decode_output_payload(remainder.lstrip())


def parse_control_mode_output_pane(line: str) -> str | None:
    """Return the pane id for a control mode output line, if any."""
    pane_id, _ = parse_control_mode_output(line)
    return pane_id or None


class ControlModeOutputMonitor:
    """Best-effort tmux control-mode monitor for pane output activity."""

    def __init__(self, session_target: str):
        self.session_target = session_target
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._proc: subprocess.Popen[bytes] | None = None
        self._master_fd: int | None = None
        self._last_output_at: dict[str, float] = {}
        self._output_buffer: dict[str, str] = {}

    def start(self) -> None:
        if not self.session_target:
            return
        with self._lock:
            if self._thread and self._thread.is_alive():
                return
            self._stop.clear()
            self._thread = threading.Thread(target=self._run, name="hive-tmux-control", daemon=True)
            self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._request_detach()
        thread = self._thread
        if thread is not None:
            thread.join(timeout=2.0)
        self._terminate_proc()

    def is_busy(self, pane_id: str, *, threshold_seconds: float) -> bool:
        if not pane_id:
            return False
        with self._lock:
            last = self._last_output_at.get(pane_id)
        if last is None:
            return False
        return (time.monotonic() - last) <= threshold_seconds

    def last_output_age(self, pane_id: str) -> float | None:
        if not pane_id:
            return None
        with self._lock:
            last = self._last_output_at.get(pane_id)
        if last is None:
            return None
        return max(0.0, time.monotonic() - last)

    def saw_msg_id(self, pane_id: str, msg_id: str) -> bool:
        if not pane_id or not msg_id:
            return False
        with self._lock:
            buffer = self._output_buffer.get(pane_id, "")
        return msg_id in buffer

    def _append_output(self, pane_id: str, payload: str) -> None:
        if not pane_id or not payload:
            return
        with self._lock:
            current = self._output_buffer.get(pane_id, "")
            combined = current + payload
            if len(combined) > _OUTPUT_BUFFER_MAX:
                combined = combined[-_OUTPUT_BUFFER_MAX:]
            self._output_buffer[pane_id] = combined

    def _request_detach(self) -> None:
        with self._lock:
            fd = self._master_fd
        if fd is None:
            return
        try:
            os.write(fd, b"detach-client\n")
        except OSError:
            pass

    def _terminate_proc(self) -> None:
        with self._lock:
            proc = self._proc
            self._proc = None
            master_fd = self._master_fd
            self._master_fd = None
        if proc is not None and proc.poll() is None:
            try:
                proc.terminate()
                proc.wait(timeout=1.0)
            except Exception:
                try:
                    proc.kill()
                except Exception:
                    pass
        if master_fd is not None:
            try:
                os.close(master_fd)
            except OSError:
                pass

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                self._run_once()
            except Exception:
                # Best-effort monitor: fall back to retry rather than crashing sidecar.
                pass
            if self._stop.is_set():
                break
            time.sleep(_CONTROL_MODE_RESTART_DELAY)

    def _run_once(self) -> None:
        master_fd, slave_fd = pty.openpty()
        proc: subprocess.Popen[bytes] | None = None
        try:
            proc = subprocess.Popen(
                ["tmux", "-C", "attach", "-t", self.session_target],
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=slave_fd,
                close_fds=True,
                start_new_session=True,
            )
        finally:
            try:
                os.close(slave_fd)
            except OSError:
                pass

        with self._lock:
            self._proc = proc
            self._master_fd = master_fd

        try:
            buffer = b""
            while not self._stop.is_set():
                if proc.poll() is not None:
                    break
                ready, _, _ = select.select([master_fd], [], [], 0.5)
                if not ready:
                    continue
                try:
                    chunk = os.read(master_fd, 65536)
                except OSError:
                    break
                if not chunk:
                    continue
                buffer += chunk
                while b"\n" in buffer:
                    raw_line, buffer = buffer.split(b"\n", 1)
                    decoded = raw_line.decode(errors="ignore").rstrip("\r")
                    pane_id, payload = parse_control_mode_output(decoded)
                    if pane_id:
                        with self._lock:
                            self._last_output_at[pane_id] = time.monotonic()
                        if payload:
                            self._append_output(pane_id, payload)
        finally:
            if proc is not None and proc.poll() is None:
                try:
                    proc.terminate()
                    proc.wait(timeout=1.0)
                except Exception:
                    try:
                        proc.kill()
                    except Exception:
                        pass
            with self._lock:
                self._proc = None
                self._master_fd = None
            try:
                os.close(master_fd)
            except OSError:
                pass


# --- Session ---

def has_session(name: str) -> bool:
    r = _run(["has-session", "-t", name], check=False)
    return r.returncode == 0


def new_session(name: str, width: int = 200, height: int = 50) -> str:
    """Create a detached tmux session. Returns the initial pane id."""
    r = _run([
        "new-session", "-d", "-s", name,
        "-x", str(width), "-y", str(height),
        "-P", "-F", "#{pane_id}",
    ])
    return r.stdout.strip()


def kill_session(name: str) -> None:
    _run(["kill-session", "-t", name], check=False)


# --- Pane ---

def split_window(
    target: str,
    horizontal: bool = True,
    size: str | None = None,
    detach: bool = True,
    cwd: str | None = None,
) -> str:
    """Split a window/pane. Returns the new pane id.

    detach=True (default) keeps focus on the original pane (-d flag).
    """
    args = ["split-window", "-t", target]
    if detach:
        args.append("-d")
    args.append("-h" if horizontal else "-v")
    if size:
        args.extend(["-l", size])
    if cwd:
        args.extend(["-c", cwd])
    args.extend(["-P", "-F", "#{pane_id}"])
    r = _run(args)
    return r.stdout.strip()


def send_keys(pane_id: str, text: str, enter: bool = True) -> None:
    """Send literal text to a pane, then optionally press Enter.

    Uses two separate tmux invocations to avoid command-chaining (;)
    interfering with literal text parsing, which caused truncation.
    """
    _run(["send-keys", "-t", pane_id, "-l", text])
    if enter:
        _run(["send-keys", "-t", pane_id, "Enter"])


def send_key(pane_id: str, key: str) -> None:
    """Send a special key (Escape, C-c, C-n, etc.)."""
    _run(["send-keys", "-t", pane_id, key])


def is_pane_in_mode(pane_id: str) -> bool:
    value = display_value(pane_id, "#{pane_in_mode}")
    return value == "1"


def cancel_pane_mode(pane_id: str) -> None:
    _run(["copy-mode", "-q", "-t", pane_id], check=False)


def capture_pane(pane_id: str, lines: int = 50) -> str:
    """Capture pane content."""
    return _run_output([
        "capture-pane", "-t", pane_id, "-p", f"-S", f"-{lines}",
    ])


def is_pane_alive(pane_id: str) -> bool:
    r = _run(
        ["list-panes", "-a", "-F", "#{pane_id} #{pane_dead}"],
        check=False,
    )
    for line in r.stdout.strip().split("\n"):
        parts = line.split()
        if len(parts) >= 2 and parts[0] == pane_id:
            return parts[1] == "0"
    return False


def kill_pane(pane_id: str) -> None:
    _run(["kill-pane", "-t", pane_id], check=False)


# --- Layout & Appearance ---

def select_layout(target: str, layout: str = "tiled") -> None:
    _run(["select-layout", "-t", target, layout], check=False)





def set_pane_title(pane_id: str, title: str) -> None:
    _run([
        "select-pane", "-t", pane_id,
        "-T", title,
    ], check=False)


_HIVE_PANE_BORDER_FORMAT = " #{?@hive-agent,#{@hive-agent},#{pane_title}} "


def enable_pane_border_status(target: str) -> None:
    """Enable pane border labels for a window.

    Hive-tagged panes show their member name; untagged panes fall back to the
    native tmux pane title.
    """
    _run([
        "set-window-option", "-t", target,
        "pane-border-status", "top",
    ], check=False)
    _run([
        "set-window-option", "-t", target,
        "pane-border-format",
        _HIVE_PANE_BORDER_FORMAT,
    ], check=False)


def set_window_option(target: str, option: str, value: str) -> None:
    _run(["set-window-option", "-t", target, option, value], check=False)


def get_window_option(target: str, key: str) -> str | None:
    r = _run(["display-message", "-t", target, "-p", f"#{{@{key}}}"], check=False)
    val = r.stdout.strip()
    return val or None


def clear_window_option(target: str, option: str) -> None:
    _run(["set-window-option", "-t", target, "-u", option], check=False)


def resize_pane(pane_id: str, width: str | None = None, height: str | None = None) -> None:
    args = ["resize-pane", "-t", pane_id]
    if width:
        args.extend(["-x", width])
    if height:
        args.extend(["-y", height])
    _run(args, check=False)


def list_panes(target: str) -> list[str]:
    """List all pane ids in a window/session."""
    r = _run(["list-panes", "-t", target, "-F", "#{pane_id}"], check=False)
    return [p for p in r.stdout.strip().split("\n") if p]


# --- Context detection ---

def is_inside_tmux() -> bool:
    return bool(os.environ.get("TMUX"))


def get_current_pane_id() -> str | None:
    """Get the pane id of the calling process (per-pane env var)."""
    return os.environ.get("TMUX_PANE")


def get_current_window_target() -> str | None:
    """Get the window target that contains the calling pane."""
    pane_id = get_current_pane_id()
    if not pane_id:
        return None
    r = _run(
        ["display-message", "-t", pane_id, "-p", "#{session_name}:#{window_index}"],
        check=False,
    )
    return r.stdout.strip() or None


def get_current_session_name() -> str | None:
    """Get the tmux session name for the calling pane."""
    pane_id = get_current_pane_id()
    if not pane_id:
        return None
    r = _run(
        ["display-message", "-t", pane_id, "-p", "#{session_name}"],
        check=False,
    )
    return r.stdout.strip() or None


def get_current_window_index() -> str | None:
    """Get the window index for the calling pane."""
    pane_id = get_current_pane_id()
    if not pane_id:
        return None
    r = _run(
        ["display-message", "-t", pane_id, "-p", "#{window_index}"],
        check=False,
    )
    return r.stdout.strip() or None


def get_current_window_id() -> str | None:
    """Get the stable tmux window id for the calling pane."""
    pane_id = get_current_pane_id()
    if not pane_id:
        return None
    return display_value(pane_id, "#{window_id}")


def display_value(target: str, fmt: str) -> str | None:
    r = _run([
        "display-message", "-t", target, "-p", fmt,
    ], check=False)
    return r.stdout.strip() or None


def supports_popup() -> bool:
    r = _run(["list-commands"], check=False)
    commands = r.stdout.strip()
    return "display-popup" in commands


def get_most_recent_client_tty(session_name: str | None = None) -> str | None:
    args = ["list-clients"]
    if session_name:
        args.extend(["-t", session_name])
    args.extend(["-F", "#{client_activity}\t#{client_tty}"])
    r = _run(args, check=False)
    rows: list[tuple[int, str]] = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t", 1)
        if len(parts) != 2 or not parts[1]:
            continue
        try:
            activity = int(parts[0] or "0")
        except ValueError:
            activity = 0
        rows.append((activity, parts[1]))
    if not rows:
        return None
    rows.sort(key=lambda item: item[0], reverse=True)
    return rows[0][1]


def get_client_window_target(client_tty: str) -> str | None:
    if not client_tty:
        return None
    r = _run(
        ["display-message", "-c", client_tty, "-p", "#{session_name}:#{window_index}"],
        check=False,
    )
    return r.stdout.strip() or None


def get_most_recent_client_window(session_name: str | None = None) -> str | None:
    client_tty = get_most_recent_client_tty(session_name)
    if not client_tty:
        return None
    return get_client_window_target(client_tty)


def get_client_mode(target: str | None = None) -> str:
    resolved_target = target or get_current_pane_id()
    if not resolved_target:
        return "unknown"
    value = display_value(resolved_target, "#{client_control_mode}")
    if value == "1":
        return "control"
    if value == "0":
        return "terminal"
    return "unknown"


def is_control_mode_client(target: str | None = None) -> bool:
    return get_client_mode(target) == "control"


def get_pane_window_name(pane_id: str) -> str | None:
    return display_value(pane_id, "#{window_name}")


def rename_window(window_target: str, name: str) -> None:
    _run(["rename-window", "-t", window_target, name], check=False)


def get_pane_tty(pane_id: str) -> str | None:
    return display_value(pane_id, "#{pane_tty}")


def get_pane_title(pane_id: str) -> str | None:
    return display_value(pane_id, "#{pane_title}")


def get_pane_current_command(pane_id: str) -> str | None:
    return display_value(pane_id, "#{pane_current_command}")


@dataclass(frozen=True)
class TTYProcessInfo:
    pid: str
    command: str
    argv: str


def list_tty_processes(tty: str) -> list[TTYProcessInfo]:
    tty_name = (tty or "").strip()
    if not tty_name:
        return []
    if tty_name.startswith("/dev/"):
        tty_name = tty_name[5:]
    try:
        result = subprocess.run(
            ["ps", "-t", tty_name, "-o", "pid=,comm=,command="],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except subprocess.TimeoutExpired:
        return []
    processes: list[TTYProcessInfo] = []
    for line in result.stdout.splitlines():
        row = line.strip()
        if not row:
            continue
        parts = row.split(None, 2)
        if len(parts) < 2:
            continue
        processes.append(TTYProcessInfo(
            pid=parts[0],
            command=parts[1],
            argv=parts[2] if len(parts) > 2 else parts[1],
        ))
    return processes


def list_open_files(pid: str) -> list[str]:
    """Return file paths held open by *pid* via ``lsof -p <pid> -Fn``."""
    if not pid:
        return []
    try:
        result = subprocess.run(
            ["lsof", "-p", str(pid), "-Fn"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return []
    paths: list[str] = []
    for line in result.stdout.splitlines():
        if line.startswith("n") and line[1:].startswith("/"):
            paths.append(line[1:])
    return paths


def list_tty_commands(tty: str) -> list[str]:
    return [process.command for process in list_tty_processes(tty)]


def get_pane_window_target(pane_id: str) -> str | None:
    return display_value(pane_id, "#{session_name}:#{window_index}")


def get_window_id(target: str) -> str | None:
    return display_value(target, "#{window_id}")


def get_pane_session_name(pane_id: str) -> str | None:
    return display_value(pane_id, "#{session_name}")


def get_pane_count(pane_id: str) -> int:
    value = display_value(pane_id, "#{window_panes}")
    try:
        return int(value or "1")
    except ValueError:
        return 1




def flash_window_status(window_target: str, style: str = "fg=colour235,bg=colour220,bold", seconds: int = 12) -> None:
    duration = max(1, int(seconds))
    interval = 0.5
    quoted_target = shlex.quote(window_target)
    quoted_style = shlex.quote(style)
    set_cmd = f"tmux set-window-option -t {quoted_target} window-status-style {quoted_style} >/dev/null 2>&1 || true"
    clear_cmd = f"tmux set-window-option -t {quoted_target} -u window-status-style >/dev/null 2>&1 || true"
    parts: list[str] = []
    for _ in range(duration):
        parts.append(set_cmd)
        parts.append(f"sleep {interval}")
        parts.append(clear_cmd)
        parts.append(f"sleep {interval}")
    parts.append(clear_cmd)
    _run(["run-shell", "-b", "; ".join(parts)], check=False)


@dataclass
class PaneInfo:
    pane_id: str
    title: str
    command: str = ""
    role: str = ""
    agent: str = ""
    team: str = ""
    cli: str = ""


def list_panes_with_titles(target: str) -> list[PaneInfo]:
    """List all panes in a window with their IDs and titles."""
    r = _run(
        ["list-panes", "-t", target, "-F", "#{pane_id}\t#{pane_title}"],
        check=False,
    )
    result = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t", 1)
        pane_id = parts[0]
        title = parts[1] if len(parts) > 1 else ""
        result.append(PaneInfo(pane_id=pane_id, title=title))
    return result


_PANE_BASE_FMT = "\t".join([
    "#{pane_id}",
    "#{pane_title}",
    "#{pane_current_command}",
    "#{@hive-role}",
    "#{@hive-agent}",
    "#{@hive-team}",
    "#{@hive-cli}",
])


def list_panes_full(target: str) -> list[PaneInfo]:
    """List all panes with command and hive identity (@hive-*)."""
    r = _run(["list-panes", "-t", target, "-F", _PANE_BASE_FMT], check=False)
    result = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t")
        while len(parts) < 7:
            parts.append("")
        result.append(PaneInfo(
            pane_id=parts[0],
            title=parts[1],
            command=parts[2],
            role=parts[3] or "",
            agent=parts[4] or "",
            team=parts[5] or "",
            cli=parts[6] or "",
        ))
    return result


# --- Per-pane user options (@hive-*) ---

def set_pane_option(pane_id: str, key: str, value: str) -> None:
    _run(["set-option", "-p", "-t", pane_id, f"@{key}", value], check=False)


def get_pane_option(pane_id: str, key: str) -> str | None:
    r = _run(["show-options", "-p", "-v", "-t", pane_id, f"@{key}"], check=False)
    if r.returncode != 0:
        return None
    val = r.stdout.strip()
    return val or None


def clear_pane_option(pane_id: str, key: str) -> None:
    _run(["set-option", "-p", "-t", pane_id, "-u", f"@{key}"], check=False)


_PANE_TAG_KEYS = ("hive-role", "hive-agent", "hive-team", "hive-cli")


def tag_pane(pane_id: str, role: str, agent: str, team: str, *, cli: str = "") -> None:
    """Set all hive identity options on a pane."""
    set_pane_option(pane_id, "hive-role", role)
    set_pane_option(pane_id, "hive-agent", agent)
    set_pane_option(pane_id, "hive-team", team)
    if cli:
        set_pane_option(pane_id, "hive-cli", cli)


def clear_pane_tags(pane_id: str) -> None:
    """Remove all hive identity options from a pane."""
    for key in _PANE_TAG_KEYS:
        clear_pane_option(pane_id, key)


# --- Utility ---

def wait_for_text(
    pane_id: str,
    text: str,
    timeout: float = 30,
    interval: float = 1,
) -> bool:
    """Wait until text appears in pane output."""
    return wait_for_texts(pane_id, (text,), timeout=timeout, interval=interval)


def wait_for_texts(
    pane_id: str,
    texts: tuple[str, ...],
    timeout: float = 30,
    interval: float = 1,
) -> bool:
    """Wait until any text appears in pane output."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        output = capture_pane(pane_id)
        if any(text in output for text in texts):
            return True
        time.sleep(interval)
    return False
