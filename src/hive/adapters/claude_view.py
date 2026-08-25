"""Which bg session a claude member's pane is actually showing.

A claude member pane is an attach *viewer*: the human can press the panel key
inside it, land in claude's own agent panel, and open any other bg session
there. The pane keeps its member identity — tags, job record, delivery
address — while the screen shows something else.

This is display truth only. A member's keystrokes are addressed to its job
and never go near the pane, so nothing here routes a delivery; it answers
what the border should label, and — via :func:`interactive_claude_pid` —
which panes are *not* viewers, the only ones tmux keys may be sent to.

Three signals, in the order they are trusted (2.1.240, real-machine verified):

- ``<claude-config>/daemon/attach-journal/<gestureId>.json``: one entry per
  attach gesture, written when a session goes on screen and removed when the
  viewer returns to the panel list or detaches (a switch removes the old
  entry and writes a new one). It names the viewer's ``pid`` and
  ``procStart``, never the target job — so it answers *whether* a session is
  displayed, never which. Entries outlive crashed viewers, hence the
  pid-alive + start-time cross-check.
- viewer argv: ``claude attach <jobId>`` names the job outright. The process
  re-execs to ``claude agents`` the moment it enters the panel and never
  names a job again — authoritative while it lasts, absent afterwards.
- ``#{pane_title}``: the panel writes the viewed session's bare name (OSC 0)
  on every switch — the only carrier of *which* in panel mode. It latches
  after the viewer dies, so it is read only behind the journal and argv
  gates. Hive member jobs are named ``<team>.<member>`` (unique per pane),
  which turns a title back into a jobId without paying for the ~270ms
  ``claude agents --json`` ledger call.

Nothing here reads ``CLAUDE_AGENTS_SELECT`` (stale after the first switch) or
the pane's current command (the claude binary is version-named, so it reads
e.g. ``2.1.240``).
"""
from __future__ import annotations

import calendar
import json
import os
import re
import shlex
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from .. import tmux
from .claude_bg import job_id_for_pane, looks_like_job_id
from .claude_sessions import _config_dir, _pid_alive

# argv[0] is the resolved binary path: `~/.local/bin/claude` normally, but the
# install is a version-named symlink tree, so a bare version basename counts too.
_VERSION_BASENAME = re.compile(r"^\d+(\.\d+)+$")
_VIEWER_SUBCOMMANDS = ("attach", "agents")
_PROC_START_FORMAT = "%a %b %d %H:%M:%S %Y"
_PROC_START_TOLERANCE = 2.0  # seconds; the two clocks are the same clock
_LABEL_MAX = 28  # a border suffix, not a log line


@dataclass(frozen=True)
class PaneView:
    """What *this* pane's viewer is showing.

    ``certainty`` is ``certain`` (process or journal evidence), ``likely``
    (the pane title named it) or ``unknown``. ``kind`` is ``member_view`` (a
    hive member's job — ``job_id``/``member`` name it), ``foreign`` (some
    other session), ``list_view`` (the panel's list, nothing displayed) or
    ``no_viewer``. ``job_id``/``member`` are empty when unresolved; ``title``
    carries the displayed session's own name when that is all there is.
    """

    certainty: str
    kind: str
    job_id: str = ""
    member: str = ""
    title: str = ""
    why: str = ""


# --------------------------------------------------------------------------
# attach journal
# --------------------------------------------------------------------------
def journal_dir() -> Path:
    return _config_dir() / "daemon" / "attach-journal"


def journal_signature() -> tuple[str, ...]:
    """Cheap change token for the journal: the entry names.

    Every attach, switch and detach adds or removes a file, so an unchanged
    tuple means no viewer changed what it displays. A missing directory
    (older claude, other config tree) signs as empty — callers then only ever
    see ``list_view``, i.e. the border simply carries no view label.
    """
    try:
        with os.scandir(journal_dir()) as entries:
            return tuple(sorted(e.name for e in entries if e.name.endswith(".json")))
    except OSError:
        return ()


def _pid_start_epoch(pid: int) -> float | None:
    """Process start time of *pid* in epoch seconds, or None."""
    try:
        result = subprocess.run(
            ["ps", "-p", str(pid), "-o", "lstart="],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    text = " ".join(result.stdout.split())
    if not text:
        return None
    try:
        return time.mktime(time.strptime(text, _PROC_START_FORMAT))
    except ValueError:
        return None


def _start_matches(claimed: str, pid: int) -> bool:
    """True when *pid* really started when the journal entry says it did."""
    # ponytail: the journal renders procStart in UTC (verified on 2.1.240)
    # while ps prints local time; both readings are accepted rather than
    # pinning a timezone the daemon never documented.
    text = " ".join((claimed or "").split())
    if not text:
        return False
    try:
        parsed = time.strptime(text, _PROC_START_FORMAT)
    except ValueError:
        return False
    actual = _pid_start_epoch(pid)
    if actual is None:
        return False
    candidates = (calendar.timegm(parsed), time.mktime(parsed))
    return any(abs(actual - candidate) <= _PROC_START_TOLERANCE for candidate in candidates)


def attach_entry_for_pid(pid: int) -> dict | None:
    """The live attach entry naming *pid* — i.e. that viewer has a session on
    screen right now — or None.

    Dead viewers leave their entries behind, so an entry only counts when its
    pid is alive *and* started when the entry recorded it (a recycled pid
    must never read as an open session).
    """
    try:
        with os.scandir(journal_dir()) as entries:
            names = [e.path for e in entries if e.name.endswith(".json")]
    except OSError:
        return None
    for path in names:
        try:
            data = json.loads(Path(path).read_text())
        except (OSError, ValueError):
            continue
        if not isinstance(data, dict) or data.get("pid") != pid:
            continue
        if not _pid_alive(pid):
            continue
        if not _start_matches(str(data.get("procStart") or ""), pid):
            continue
        return data
    return None


# --------------------------------------------------------------------------
# viewer process + hive member index
# --------------------------------------------------------------------------
def _viewer_argv(argv: str) -> tuple[str, str] | None:
    """``(subcommand, first argument)`` for a claude viewer argv, else None.

    Hidden subcommands are only recognized at argv[1], which is also what
    makes this safe: an argv that merely mentions "claude attach" (a grep, an
    editor) never matches.
    """
    try:
        parts = shlex.split(argv or "")
    except ValueError:
        parts = (argv or "").split()
    if len(parts) < 2:
        return None
    base = os.path.basename(parts[0])
    if base != "claude" and not _VERSION_BASENAME.match(base):
        return None
    if parts[1] not in _VIEWER_SUBCOMMANDS:
        return None
    return parts[1], parts[2] if len(parts) > 2 else ""


def viewer_for_pane(pane_id: str) -> tuple[int, str, str] | None:
    """``(pid, subcommand, argument)`` of the claude viewer on *pane_id*'s
    tty, or None when no viewer is running there.

    The engine (argv ``claude bg-spare``) lives on claude's own supervisor,
    never on a pane tty, so it can never match here.
    """
    try:
        tty = tmux.get_pane_tty(pane_id) or ""
        for process in tmux.list_tty_processes(tty):
            parsed = _viewer_argv(process.argv)
            if parsed is None:
                continue
            return int(process.pid), parsed[0], parsed[1]
    except (OSError, ValueError):
        return None
    return None


def interactive_claude_pid(pane_id: str) -> int | None:
    """Pid of a *plain interactive* claude TUI on *pane_id*'s tty, or None.

    The only shape tmux keystrokes may be typed into. An attach viewer is a
    claude process on the tty too, but its keyboard belongs to whichever
    session it currently displays — another member's, or one the human opened
    from the panel — so keys sent there land in a stranger's composer. Hive
    members never reach this: their keystrokes are addressed to the job.
    """
    from ..agent_cli import claude_pid_for_pane

    pid = claude_pid_for_pane(pane_id)
    if pid is None or viewer_for_pane(pane_id) is not None:
        return None
    return pid


def member_job_index(panes: list[tmux.PaneInfo] | None = None) -> dict[str, str]:
    """``{"<team>.<member>": jobId}`` for every claude member pane on the
    server — the job *name* a member's engine registers under, which is what
    the panel writes into the pane title.

    The name is rebuilt from the pane tags rather than read from the ledger.
    That holds because ``hive claude`` mints a member's job under
    ``<team>.<member>``; a pane rebound with ``--resume`` to a job minted
    under some other name is only unresolvable *by title* (the argv branch
    matches by job id), which costs a border label, nothing more.
    """
    rows = tmux.list_panes_all() if panes is None else panes
    index: dict[str, str] = {}
    for pane in rows:
        if pane.cli != "claude" or not pane.agent or not pane.team:
            continue
        job_id = job_id_for_pane(pane.pane_id)
        if job_id:
            index[f"{pane.team}.{pane.agent}"] = job_id
    return index


def _title_names(title: str, name: str) -> bool:
    """True when *title* carries *name* as a whole token.

    The panel writes the bare session name and may decorate it (spinner,
    counters, and tmux flattens every non-ASCII byte to '_'), so equality is
    too strict — but containment is too loose: a foreign session named
    ``probe.red-notes`` would then read as member ``probe.red`` and
    keystrokes would land in the wrong session. A name character on either
    side (``\\w``, ``.`` or ``-``) means the title names something else, which
    also keeps prefix siblings (``probe.red`` vs ``probe.red2``) apart.
    """
    return re.search(rf"(?:^|[^\w.-]){re.escape(name)}(?:[^\w.-]|$)", title) is not None


def view_for_pane(pane_id: str, *, panes: list[tmux.PaneInfo] | None = None) -> PaneView:
    """What *pane_id* is displaying right now.

    Pass *panes* (a ``tmux.list_panes_all()`` result) when the caller already
    has one; the member index is built from it.
    """
    viewer = viewer_for_pane(pane_id)
    if viewer is None:
        # The title is a latched leftover of whatever the dead viewer showed
        # last — never evidence that anything is on screen.
        return PaneView("certain", "no_viewer", why="no claude viewer on the pane tty")
    pid, subcommand, argument = viewer
    if attach_entry_for_pid(pid) is None:
        return PaneView("certain", "list_view", why=f"viewer {pid} has no live attach entry")

    index = member_job_index(panes)
    if subcommand == "attach" and looks_like_job_id(argument):
        member = next((name for name, job in index.items() if job == argument), "")
        return PaneView(
            certainty="certain",
            kind="member_view" if member else "foreign",
            job_id=argument,
            member=member,
            why=f"viewer {pid} argv still names the job",
        )

    title = (tmux.get_pane_title(pane_id) or "").strip()
    if not title:
        return PaneView(
            certainty="unknown",
            kind="foreign",
            why=f"viewer {pid} has a session open, title empty",
        )
    matches = [name for name in index if _title_names(title, name)]
    if len(matches) == 1:
        return PaneView(
            certainty="likely",
            kind="member_view",
            job_id=index[matches[0]],
            member=matches[0],
            title=title,
            why=f"title {title!r} names the member",
        )
    if not matches:
        return PaneView(
            certainty="likely",
            kind="foreign",
            title=title,
            why=f"title {title!r} is no hive member",
        )
    return PaneView(
        certainty="unknown",
        kind="foreign",
        title=title,
        why=f"title {title!r} matched {len(matches)} members",
    )


def view_label(view: PaneView, own_job_id: str) -> str:
    """Border suffix for *view*: what to show after the member's own name.

    Empty means "nothing to add" — the pane shows its own member, the panel
    list, or nothing identifiable. ``#`` is stripped so a session name can
    never inject a tmux format into the border.
    """
    if view.job_id and view.job_id == own_job_id:
        return ""
    if view.kind == "member_view" and view.member:
        label = view.member
    elif view.kind == "foreign" and view.certainty == "likely":
        label = view.title
    else:
        return ""
    return label.replace("#", "").strip()[:_LABEL_MAX]
