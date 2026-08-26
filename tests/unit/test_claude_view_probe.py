"""Which bg session a claude member pane is showing.

The probe ranks three signals — viewer argv, the attach journal, the pane
title — and every branch here is a state a human can put a member pane into
by browsing the attach panel.
"""

import json
import os
import subprocess
import time

import pytest

from hive import tmux
from hive.adapters import claude_bg, claude_view

pytestmark = pytest.mark.unit

PANE = "%7"
JOB = "cafe1234"
OTHER_JOB = "beef5678"
DEAD_PID = 4242424  # out of range on macOS/Linux: never a live process


def _pane(pane_id: str, *, agent: str, team: str, cli: str = "claude") -> tmux.PaneInfo:
    return tmux.PaneInfo(
        pane_id=pane_id, title="", command="2.1.240", role="agent",
        agent=agent, team=team, cli=cli, group="",
    )


@pytest.fixture
def claude_home(tmp_path, monkeypatch):
    """An isolated claude config tree: pane job records and attach journal."""
    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    (tmp_path / "daemon" / "attach-journal").mkdir(parents=True)
    claude_bg.write_pane_job(PANE, JOB, "session-1", "/tmp")
    return tmp_path


def _proc_start_utc(pid: int) -> str:
    """A journal entry renders the start time in UTC; ps prints local time."""
    out = subprocess.run(
        ["ps", "-p", str(pid), "-o", "lstart="], capture_output=True, text=True
    ).stdout
    epoch = time.mktime(time.strptime(" ".join(out.split()), "%a %b %d %H:%M:%S %Y"))
    return time.strftime("%a %b %d %H:%M:%S %Y", time.gmtime(epoch))


def _journal(home, *, pid: int, proc_start: str = "", name: str = "gesture") -> None:
    (home / "daemon" / "attach-journal" / f"{name}.json").write_text(json.dumps({
        "gestureId": name,
        "surface": "bg_cli",
        "startedAtEpochMs": 1787651900942,
        "pid": pid,
        "procStart": proc_start or _proc_start_utc(pid),
    }))


@pytest.fixture
def probe(monkeypatch):
    """Drive the probe's tmux inputs: viewer argv, pane title, member panes.

    The viewer pid defaults to this test process so journal liveness and
    start-time checks run against a real process.
    """
    state = {
        "argv": "",
        "title": "",
        "viewer_pid": os.getpid(),
        "panes": [_pane(PANE, agent="red", team="probe")],
    }

    def _processes(_tty):
        if not state["argv"]:
            return [tmux.TTYProcessInfo(pid="99", command="-zsh", argv="-zsh")]
        return [tmux.TTYProcessInfo(
            pid=str(state["viewer_pid"]), command="2.1.240", argv=state["argv"]
        )]

    monkeypatch.setattr("hive.adapters.claude_view.tmux.get_pane_tty", lambda _p: "/dev/ttys012")
    monkeypatch.setattr("hive.adapters.claude_view.tmux.list_tty_processes", _processes)
    monkeypatch.setattr("hive.adapters.claude_view.tmux.get_pane_title", lambda _p: state["title"])
    monkeypatch.setattr("hive.adapters.claude_view.tmux.list_panes_all", lambda: state["panes"])
    return state


# --- the states a member pane can be in -----------------------------------


def test_no_viewer_discards_the_latched_title(probe, claude_home):
    # The title latches after the viewer dies: it is not evidence of a view.
    probe["title"] = "probe.red"
    _journal(claude_home, pid=os.getpid())

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind, view.job_id) == ("certain", "no_viewer", "")


def test_attach_argv_names_the_job(probe, claude_home):
    probe["argv"] = "claude attach cafe1234"
    probe["title"] = "stale nonsense"
    _journal(claude_home, pid=os.getpid())

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind) == ("certain", "member_view")
    assert (view.job_id, view.member) == (JOB, "probe.red")


def test_attach_argv_of_a_job_hive_does_not_own_is_foreign(probe, claude_home):
    probe["argv"] = "claude attach beef5678"
    _journal(claude_home, pid=os.getpid())

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind, view.job_id) == ("certain", "foreign", OTHER_JOB)


def test_panel_without_a_journal_entry_is_the_list_view(probe, claude_home):
    # Back on the panel list: the entry is gone, the title still names the
    # session that was open a moment ago.
    probe["argv"] = "claude agents"
    probe["title"] = "probe.red"

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind, view.job_id) == ("certain", "list_view", "")


def test_panel_with_an_entry_resolves_the_member_from_the_title(probe, claude_home):
    probe["argv"] = "claude agents"
    probe["title"] = "probe.red"
    _journal(claude_home, pid=os.getpid())

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind) == ("likely", "member_view")
    assert (view.job_id, view.member) == (JOB, "probe.red")


def test_panel_title_may_be_decorated_by_the_tui(probe, claude_home):
    # tmux flattens every non-ASCII byte in a title to '_'.
    probe["argv"] = "claude agents"
    probe["title"] = "_ probe.red _ 3 messages"
    _journal(claude_home, pid=os.getpid())

    view = claude_view.view_for_pane(PANE)

    assert (view.kind, view.member) == ("member_view", "probe.red")


def test_a_session_that_is_no_hive_member_is_foreign(probe, claude_home):
    probe["argv"] = "claude agents"
    probe["title"] = "someone-elses-job"
    _journal(claude_home, pid=os.getpid())

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind, view.job_id) == ("likely", "foreign", "")
    assert view.title == "someone-elses-job"


@pytest.fixture
def sibling_member(probe, claude_home):
    """A second member whose name has this pane's member as a prefix."""
    probe["argv"] = "claude agents"
    probe["panes"] = [
        _pane(PANE, agent="red", team="probe"),
        _pane("%8", agent="red2", team="probe"),
    ]
    claude_bg.write_pane_job("%8", OTHER_JOB, "session-2", "/tmp")
    _journal(claude_home, pid=os.getpid())
    return probe


def test_a_prefix_named_sibling_resolves_to_itself(sibling_member):
    sibling_member["title"] = "probe.red2"

    view = claude_view.view_for_pane(PANE)

    assert (view.kind, view.member, view.job_id) == ("member_view", "probe.red2", OTHER_JOB)


def test_a_title_naming_two_members_resolves_to_nothing(sibling_member):
    sibling_member["title"] = "probe.red probe.red2"

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind, view.job_id) == ("unknown", "foreign", "")


@pytest.mark.parametrize("title", ["probe.red-notes", "probe.reduce", "xprobe.red"])
def test_a_foreign_name_that_merely_contains_a_member_is_foreign(probe, claude_home, title):
    # The hole a containment match would leave: keystrokes meant for
    # probe.red would land in this stranger's session.
    probe["argv"] = "claude agents"
    probe["title"] = title
    _journal(claude_home, pid=os.getpid())

    view = claude_view.view_for_pane(PANE)

    assert (view.certainty, view.kind, view.job_id) == ("likely", "foreign", "")


def test_argument_text_is_never_identity(probe, claude_home):
    # A grep for the attach command line is not a viewer.
    probe["argv"] = "rg claude attach src"
    _journal(claude_home, pid=os.getpid())

    assert claude_view.view_for_pane(PANE).kind == "no_viewer"


# --- journal residue ------------------------------------------------------


def test_entry_of_a_dead_viewer_is_residue(probe, claude_home):
    # Recycled pid: the entry names this viewer's pid, but that process is
    # long gone — the journal is full of such leftovers.
    probe["argv"] = "claude agents"
    probe["title"] = "probe.red"
    probe["viewer_pid"] = DEAD_PID
    _journal(claude_home, pid=DEAD_PID, proc_start="Tue Aug 25 09:58:20 2026")

    assert claude_view.view_for_pane(PANE).kind == "list_view"


def test_entry_whose_start_time_does_not_match_is_residue(probe, claude_home):
    probe["argv"] = "claude agents"
    probe["title"] = "probe.red"
    _journal(claude_home, pid=os.getpid(), proc_start="Tue Aug 25 09:58:20 2026")

    assert claude_view.view_for_pane(PANE).kind == "list_view"


def test_a_missing_journal_directory_degrades_to_list_view(probe, claude_home):
    probe["argv"] = "claude agents"
    probe["title"] = "probe.red"
    (claude_home / "daemon" / "attach-journal").rmdir()

    assert claude_view.journal_signature() == ()
    assert claude_view.view_for_pane(PANE).kind == "list_view"


def test_journal_signature_tracks_entries(claude_home):
    assert claude_view.journal_signature() == ()
    _journal(claude_home, pid=os.getpid(), name="one")
    assert claude_view.journal_signature() == ("one.json",)
    _journal(claude_home, pid=os.getpid(), name="two")
    assert claude_view.journal_signature() == ("one.json", "two.json")


# --- border label ---------------------------------------------------------


def test_view_label_is_empty_on_the_pane_s_own_member():
    view = claude_view.PaneView("certain", "member_view", JOB, "probe.red")
    assert claude_view.view_label(view, JOB) == ""


def test_view_label_names_another_member_and_a_foreign_session():
    other = claude_view.PaneView("likely", "member_view", OTHER_JOB, "comb.blue")
    assert claude_view.view_label(other, JOB) == "comb.blue"
    foreign = claude_view.PaneView("likely", "foreign", title="someone-elses-job")
    assert claude_view.view_label(foreign, JOB) == "someone-elses-job"


def test_view_label_says_nothing_when_no_session_is_displayed():
    for kind in ("list_view", "no_viewer"):
        assert claude_view.view_label(claude_view.PaneView("certain", kind), JOB) == ""
    unknown = claude_view.PaneView("unknown", "foreign", title="whatever")
    assert claude_view.view_label(unknown, JOB) == ""


def test_view_label_cannot_inject_a_tmux_format():
    view = claude_view.PaneView("likely", "foreign", title="#{pane_title}")
    assert "#" not in claude_view.view_label(view, JOB)
