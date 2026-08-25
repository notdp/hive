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
_OCTAL_DIGITS = frozenset("01234567")
_ANSI_ESCAPE_RE = re.compile(
    r"\x1b(?:"
    r"\[[0-?]*[ -/]*[@-~]|"
    r"\][^\x07]*?(?:\x07|\x1b\\)|"
    r"P.*?(?:\x1b\\)|"
    r"[@-Z\\-_]"
    r")",
    re.DOTALL,
)
_CONTROL_CHARS_RE = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")


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


def _control_mode_payload_has_activity(payload: str) -> bool:
    """Return true when payload contains visible text, not only terminal repaint codes."""
    if not payload:
        return False
    visible = _ANSI_ESCAPE_RE.sub("", payload)
    visible = _CONTROL_CHARS_RE.sub("", visible)
    return bool(visible.strip())


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

    def _record_control_mode_output(self, pane_id: str, payload: str) -> None:
        if not pane_id:
            return
        if not _control_mode_payload_has_activity(payload):
            return
        with self._lock:
            self._last_output_at[pane_id] = time.monotonic()

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
                        self._record_control_mode_output(pane_id, payload)
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


def new_window(
    session: str,
    *,
    name: str = "",
    cwd: str | None = None,
    detach: bool = True,
    index: int | None = None,
) -> tuple[str, str]:
    """Create a new tmux window in *session*. Returns (window_target, pane_id).

    If *index* is given, the new window is created at that explicit tmux
    window index via `-t session:index`. Caller must ensure the index is
    free — tmux refuses with "index N in use" otherwise. Used by squad
    spawn-duo to place peer windows at 1000+ so they never collide with
    the user's regular low-index windows.
    """
    if index is not None:
        target = f"{session}:{index}"
    else:
        # Force `-t` to reference a session, not a window index. Bare numeric
        # session names (e.g. "613") are ambiguous and tmux can treat `-t 613`
        # as an index rather than a session, which fails with "index N in use"
        # once any window exists at that index.
        target = session if (":" in session or session.startswith("$")) else f"{session}:"
    args = ["new-window", "-t", target]
    if detach:
        args.append("-d")
    if name:
        args.extend(["-n", name])
    if cwd:
        args.extend(["-c", cwd])
    args.extend(["-P", "-F", "#{session_name}:#{window_index}\t#{pane_id}"])
    r = _run(args)
    out = r.stdout.strip()
    if "\t" not in out:
        return out, ""
    target, pane_id = out.split("\t", 1)
    return target, pane_id


def break_pane(pane_id: str, *, name: str = "", detach: bool = True) -> tuple[str, str]:
    """Break *pane_id* out into its own new window. Returns (window_target, pane_id).

    The pane's running process (e.g. agent CLI) continues — only its window
    parent changes.
    """
    args = ["break-pane", "-s", pane_id]
    if detach:
        args.append("-d")
    if name:
        args.extend(["-n", name])
    args.extend(["-P", "-F", "#{session_name}:#{window_index}\t#{pane_id}"])
    r = _run(args)
    out = r.stdout.strip()
    if "\t" not in out:
        return out, pane_id
    target, new_pane_id = out.split("\t", 1)
    return target, new_pane_id or pane_id


def join_pane(source_pane: str, target_pane: str, *, horizontal: bool = True, size: str | None = None) -> str:
    """Move *source_pane* into the window owning *target_pane* via tmux join-pane.

    The moved pane keeps its process and pane_id; only its window parent
    changes. Returns the (unchanged) source pane_id.
    """
    args = ["join-pane", "-s", source_pane, "-t", target_pane]
    args.append("-h" if horizontal else "-v")
    if size:
        args.extend(["-l", size])
    _run(args, check=False)
    return source_pane


def window_size(window_target: str) -> tuple[int, int]:
    """Return (width, height) for *window_target*, or (0, 0) on error."""
    r = _run(
        ["display-message", "-t", window_target, "-p", "#{window_width}\t#{window_height}"],
        check=False,
    )
    out = r.stdout.strip()
    if "\t" not in out:
        return 0, 0
    try:
        w, h = out.split("\t", 1)
        return int(w), int(h)
    except ValueError:
        return 0, 0


def select_window(window_target: str) -> None:
    _run(["select-window", "-t", window_target], check=False)


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


def send_keys_batch(pane_id: str, *keys: str) -> None:
    """Send multiple keys in one tmux call (atomic w.r.t. tmux server)."""
    if not keys:
        return
    _run(["send-keys", "-t", pane_id, *keys])


def get_cursor_x(pane_id: str) -> int | None:
    value = display_value(pane_id, "#{cursor_x}")
    try:
        return int(value) if value is not None else None
    except (TypeError, ValueError):
        return None


def load_buffer(name: str, data: str) -> None:
    """Load data into a named tmux buffer via stdin."""
    try:
        subprocess.run(
            ["tmux", "load-buffer", "-b", name, "-"],
            input=data,
            text=True,
            check=True,
            timeout=5,
        )
    except subprocess.TimeoutExpired:
        pass


def paste_buffer(name: str, target: str, *, bracketed: bool = False) -> None:
    """Paste a named tmux buffer into a pane (optionally with bracketed-paste sequences)."""
    args = ["paste-buffer", "-b", name, "-t", target]
    if bracketed:
        args.insert(1, "-p")
    _run(args, check=False)


def delete_buffer(name: str) -> None:
    _run(["delete-buffer", "-b", name], check=False)


def is_pane_in_mode(pane_id: str) -> bool:
    value = display_value(pane_id, "#{pane_in_mode}")
    return value == "1"


def cancel_pane_mode(pane_id: str) -> None:
    _run(["copy-mode", "-q", "-t", pane_id], check=False)


def capture_pane(pane_id: str, lines: int = 50, *, preserve_styles: bool = False) -> str:
    """Capture pane content."""
    args = ["capture-pane", "-t", pane_id]
    if preserve_styles:
        args.append("-e")
    args.extend(["-p", f"-S", f"-{lines}"])
    return _run_output(args)


def is_pane_alive(pane_id: str) -> bool:
    r = _run(
        ["list-panes", "-a", "-F", "#{pane_id} #{pane_dead}"],
        check=False,
    )
    if r.returncode != 0:
        # tmux didn't answer (timeout / transient failure): unknown is not
        # dead. Callers take irreversible action on False (daemon reap, team
        # GC), so only a successful listing may declare a pane dead.
        return True
    for line in r.stdout.strip().split("\n"):
        parts = line.split()
        if len(parts) >= 2 and parts[0] == pane_id:
            return parts[1] == "0"
    return False


def kill_pane(pane_id: str) -> None:
    _run(["kill-pane", "-t", pane_id], check=False)


def kill_window(target: str) -> None:
    _run(["kill-window", "-t", target], check=False)


# --- Layout & Appearance ---

def select_layout(target: str, layout: str = "tiled") -> None:
    _run(["select-layout", "-t", target, layout], check=False)





def set_pane_title(pane_id: str, title: str) -> None:
    _run([
        "select-pane", "-t", pane_id,
        "-T", title,
    ], check=False)


# A claude member pane is an attach *viewer*: the human can switch it to
# another bg session while the pane keeps its member tags. The sidecar's view
# probe writes what is really on screen into `@hive-view` (empty while the
# pane shows its own member), so the border reads "name -> what you are
# actually looking at" without the format having to guess from the title.
_HIVE_PANE_BORDER_FORMAT = (
    " #{?@hive-notify-active,#[fg=colour220]#[bold][!] #[default],}"
    "#{?@hive-agent,#{@hive-agent}"
    "#{?@hive-view,#[fg=colour220] -> #{@hive-view}#[default],}"
    ",#{pane_title}} "
)


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


def configure_hive_window(target: str) -> None:
    """Apply tmux window options expected for Hive-managed panes."""
    enable_pane_border_status(target)
    set_window_option(target, "monitor-activity", "off")
    set_window_option(target, "monitor-bell", "off")


def set_window_option(target: str, option: str, value: str) -> None:
    _run(["set-window-option", "-t", target, option, value], check=False)


def get_window_option(target: str, key: str) -> str | None:
    r = _run(["display-message", "-t", target, "-p", f"#{{@{key}}}"], check=False)
    val = r.stdout.strip()
    return val or None


def get_global_window_option(option: str) -> str | None:
    """Global (server-wide) window-option value — read-only, no target.

    Values keep their exact spacing (status formats carry meaningful
    leading/trailing padding); only the trailing newline is removed.
    """
    r = _run(["show-options", "-w", "-g", "-v", option], check=False)
    val = r.stdout.rstrip("\n")
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
    """True inside a tmux client — or inside a member engine's tool subprocess.

    A claude bg engine runs on the supervisor's pty, not in any tmux client,
    so its tools see no reliable $TMUX; but the member's pane identity is
    resolvable from the engine's own env markers, and the tmux server on the
    default socket answers targeted commands without $TMUX. Gating on $TMUX
    alone would lock every member out of hive.
    """
    if os.environ.get("TMUX"):
        return True
    return _member_env_pane() is not None


def _member_env_pane() -> str | None:
    """Pane resolved from a member engine's per-tool env markers, or None.

    - codex injects the thread's ``CODEX_THREAD_ID`` into tool subprocesses;
      hive records which pane each thread is bound to.
    - a claude bg engine's tools carry ``CLAUDE_CODE_MESSAGING_SOCKET``
      (``/tmp/cc-socks/<enginePid>.sock``); the engine's registry entry names
      its jobId, and hive records which pane each job is bound to. An
      interactive claude session's tools carry the socket too, but have no
      bg registry entry (and no job record), so they fall through.
    """
    thread_id = os.environ.get("CODEX_THREAD_ID", "").strip()
    if thread_id:
        from .adapters.codex_app_server import pane_for_thread

        pane = pane_for_thread(thread_id)
        if pane:
            return pane
    sock = os.environ.get("CLAUDE_CODE_MESSAGING_SOCKET", "").strip()
    if sock:
        stem = os.path.basename(sock).rsplit(".", 1)[0]
        if stem.isdigit():
            from .adapters.claude_bg import engine_session_for_pid, pane_for_job

            engine = engine_session_for_pid(int(stem))
            if engine is not None:
                pane = pane_for_job(engine.job_id)
                if pane:
                    return pane
    return None


def get_current_pane_id() -> str | None:
    """Get the pane id of the calling process.

    Inside a member engine's tool subprocess the env's TMUX_PANE is
    unreliable — the codex shared daemon's env is frozen at spawn time (and
    hive strips TMUX_PANE from it), and a claude bg engine has none at all —
    so the per-CLI identity markers win over the env var (see
    :func:`_member_env_pane`); everywhere else the per-pane TMUX_PANE env var
    is the answer.
    """
    pane = _member_env_pane()
    if pane:
        return pane
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


def window_exists(window_id: str) -> bool:
    """True only when tmux resolves ``window_id`` to itself.

    Never raises: a missing tmux binary, timeout, nonzero exit, or mismatched
    id all mean "not alive" to callers making reap decisions.
    """
    if not window_id:
        return False
    try:
        r = _run(["display-message", "-t", window_id, "-p", "#{window_id}"], check=False)
    except OSError:
        return False
    return r.returncode == 0 and r.stdout.strip() == window_id


def display_popup(target: str, shell_command: str, *, client: str = "",
                  x: str = "", y: str = "", width: str = "", height: str = "",
                  borderless: bool = False, close_on_exit: bool = False,
                  timeout: int = 5) -> None:
    """Open a popup over ``target`` running ``shell_command``. Never raises."""
    args = ["display-popup"]
    if client:
        args += ["-c", client]
    args += ["-t", target]
    if borderless:
        args.append("-B")
    if x:
        args += ["-x", x]
    if y:
        args += ["-y", y]
    if width:
        args += ["-w", width]
    if height:
        args += ["-h", height]
    if close_on_exit:
        args.append("-E")
    args.append(shell_command)
    try:
        _run(args, check=False, timeout=timeout)
    except OSError:
        pass


def run_shell_detached(command: str) -> None:
    """``run-shell -b <command>``: the shell string is passed byte-for-byte."""
    _run(["run-shell", "-b", command], check=False)


def source_file(path: str) -> bool:
    """Source a tmux conf; False on missing tmux, timeout, or nonzero exit."""
    try:
        r = _run(["source-file", path], check=False)
    except OSError:
        return False
    return r.returncode == 0


def get_most_recent_client_tty(session_name: str | None = None) -> str | None:
    rows = _list_terminal_clients(session_name)
    return rows[0][2] if rows else None


def get_most_recent_terminal_client_pane(session_name: str | None = None) -> str | None:
    rows = _list_terminal_clients(session_name)
    return rows[0][1] if rows else None


def _list_terminal_clients(session_name: str | None = None) -> list[tuple[int, str, str]]:
    args = ["list-clients"]
    if session_name:
        args.extend(["-t", session_name])
    args.extend(["-F", "#{client_activity}\t#{client_control_mode}\t#{pane_id}\t#{client_tty}"])
    r = _run(args, check=False)
    rows: list[tuple[int, str, str]] = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) != 4 or parts[1] != "0" or not parts[2] or not parts[3]:
            continue
        try:
            activity = int(parts[0] or "0")
        except ValueError:
            activity = 0
        rows.append((activity, parts[2], parts[3]))
    rows.sort(key=lambda item: item[0], reverse=True)
    return rows


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


def _list_open_files_lsof(pid: str) -> list[str]:
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


def list_open_files(pid: str) -> list[str]:
    """Return file paths held open by *pid*.

    Darwin uses the native ``proc_pidinfo`` provider first; other platforms and
    native failures fall back to ``lsof``.
    """
    if not pid:
        return []
    try:
        from . import proc_info

        return proc_info.list_open_files(pid)
    except Exception:
        return _list_open_files_lsof(pid)


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
    group: str = ""
    owner: str = ""


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


# Field tables drive both the tmux format string and the parser: adding a
# column means adding one row here — no count literals to keep in sync.
_PANE_FIELDS: tuple[tuple[str, str], ...] = (
    ("pane_id", "#{pane_id}"),
    ("title", "#{pane_title}"),
    ("command", "#{pane_current_command}"),
    ("role", "#{@hive-role}"),
    ("agent", "#{@hive-agent}"),
    ("team", "#{@hive-team}"),
    ("cli", "#{@hive-cli}"),
    ("group", "#{@hive-group}"),
    ("owner", "#{@hive-owner}"),
)
_PANE_BASE_FMT = "\t".join(fmt for _name, fmt in _PANE_FIELDS)

_TEAM_WINDOW_FIELDS: tuple[tuple[str, str], ...] = (
    ("window", "#{session_name}:#{window_index}"),
    ("windowName", "#{window_name}"),
    ("windowId", "#{window_id}"),
    ("team", "#{@hive-team}"),
    ("workspace", "#{@hive-workspace}"),
    ("created", "#{@hive-created}"),
    ("pr", "#{@hive-pr}"),
)
_TEAM_WINDOW_FMT = "\t".join(fmt for _name, fmt in _TEAM_WINDOW_FIELDS)


def _split_fields(line: str, fields: tuple[tuple[str, str], ...]) -> dict[str, str]:
    parts = line.split("\t")
    while len(parts) < len(fields):
        parts.append("")
    return {name: parts[i] for i, (name, _fmt) in enumerate(fields)}


def list_panes_full(target: str) -> list[PaneInfo]:
    """List all panes with command and hive identity (@hive-*)."""
    panes = list_panes_full_or_none(target)
    return panes if panes is not None else []


def list_panes_full_or_none(target: str) -> list[PaneInfo] | None:
    """Status-aware ``list_panes_full``: None when tmux did not answer.

    A successful-but-empty listing is a real empty window; None means the
    caller cannot tell missing panes from a transient tmux failure and must
    not take irreversible action on it (same contract as ``is_pane_alive``).
    """
    r = _run(["list-panes", "-t", target, "-F", _PANE_BASE_FMT], check=False)
    if r.returncode != 0:
        return None
    return _parse_panes_full(r.stdout)


def list_panes_all() -> list[PaneInfo]:
    """List every pane across all sessions/windows with hive identity tags."""
    r = _run(["list-panes", "-a", "-F", _PANE_BASE_FMT], check=False)
    return _parse_panes_full(r.stdout)


def _stderr_means_no_server(stderr: str | None) -> bool:
    """True only when tmux stderr proves there is no server to talk to.

    Proven messages: "no server running on <path>" (clean shutdown) and
    "error connecting to <path> (No such file or directory)" (socket gone).
    Anything else — permission denied, connection refused, unexpected text —
    stays unknown: a server may well be alive behind the failure.
    """
    low = (stderr or "").lower()
    if "no server running" in low:
        return True
    return "error connecting" in low and "no such file or directory" in low


def list_panes_all_status() -> tuple[list[PaneInfo] | None, str]:
    """Status-aware ``list_panes_all``: ``(panes, "ok")`` on success.

    ``(None, "no-server")`` when no tmux server is running (nothing can be
    live), ``(None, "unknown")`` on any other failure — callers must not
    read unknown as "dead" (same contract as ``is_pane_alive``).
    """
    r = _run(["list-panes", "-a", "-F", _PANE_BASE_FMT], check=False)
    if r.returncode == 0:
        return _parse_panes_full(r.stdout), "ok"
    if _stderr_means_no_server(r.stderr):
        return None, "no-server"
    return None, "unknown"


def list_team_windows_status() -> tuple[list[dict[str, str]] | None, str]:
    """Status-aware scan of windows carrying ``@hive-team``.

    Same (value, status) contract as :func:`list_panes_all_status`. Each
    entry: window target/name/id plus the team, workspace, and created
    options — everything `hive ls` / `hive resume` need to match a live
    team instance against a snapshot.
    """
    r = _run(["list-windows", "-a", "-F", _TEAM_WINDOW_FMT], check=False)
    if r.returncode != 0:
        if _stderr_means_no_server(r.stderr):
            return None, "no-server"
        return None, "unknown"
    out: list[dict[str, str]] = []
    for line in r.stdout.strip().split("\n"):
        if not line:
            continue
        entry = _split_fields(line, _TEAM_WINDOW_FIELDS)
        if entry["team"]:
            out.append(entry)
    return out, "ok"


def list_window_indices(session: str) -> list[int]:
    """Return tmux window indices in *session*, ignoring non-numeric output."""
    r = _run(["list-windows", "-t", session, "-F", "#{window_index}"], check=False)
    out: list[int] = []
    for line in r.stdout.strip().split("\n"):
        line = line.strip()
        if not line.isdigit():
            continue
        out.append(int(line))
    return out


def list_window_names() -> list[tuple[str, str]]:
    """Return ``(window_target, window_name)`` for every window across sessions."""
    r = _run(["list-windows", "-a", "-F", "#{session_name}:#{window_index}\t#{window_name}"], check=False)
    out: list[tuple[str, str]] = []
    for line in r.stdout.strip().split("\n"):
        if "\t" in line:
            target, name = line.split("\t", 1)
            out.append((target, name))
    return out


def _parse_panes_full(stdout: str) -> list[PaneInfo]:
    result: list[PaneInfo] = []
    for line in stdout.strip().split("\n"):
        if not line:
            continue
        result.append(PaneInfo(**_split_fields(line, _PANE_FIELDS)))
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


# `hive-view` is derived state (the claude view probe writes it), not
# identity — but release must clear it with the rest, or a reused pane keeps
# rendering a border suffix nobody owns any more.
_PANE_TAG_KEYS = (
    "hive-role", "hive-agent", "hive-team", "hive-cli", "hive-group", "hive-owner", "hive-view",
)


def tag_pane(pane_id: str, role: str, agent: str, team: str, *, cli: str = "", group: str = "") -> None:
    """Set all hive identity options on a pane."""
    set_pane_option(pane_id, "hive-role", role)
    set_pane_option(pane_id, "hive-agent", agent)
    set_pane_option(pane_id, "hive-team", team)
    if cli:
        set_pane_option(pane_id, "hive-cli", cli)
        if cli != "claude":
            # Only the claude view tick maintains `hive-view`, and it skips
            # non-claude panes — so a pane retagged onto another CLI in place
            # would keep its last ' -> <session>' suffix forever.
            clear_pane_option(pane_id, "hive-view")
    if group:
        set_pane_option(pane_id, "hive-group", group)


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
