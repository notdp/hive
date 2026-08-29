"""The view tick: what a member pane shows follows the human's panel switches.

Border cosmetics live in a tmux pane option the border format reads, so the
tick's job is to keep `@hive-view` honest — and to log a switch onto another
hive member, which is what a whole-window follow would key on later.
"""
from types import SimpleNamespace

import pytest

import hive.hived as hived
from hive.adapters.claude_view import PaneView

pytestmark = pytest.mark.unit

MEMBERS = {"red": {"name": "red", "pane": "%1", "cli": "claude", "role": "agent"}}


def _pane(pane_id="%1", *, title="", cli="claude"):
    return SimpleNamespace(pane_id=pane_id, title=title, cli=cli)


@pytest.fixture
def tick(monkeypatch):
    """Wire the tick's inputs; collect the tmux options it sets."""
    env = {
        "panes": [_pane()],
        "signature": ("one.json",),
        "view": PaneView("certain", "member_view", "cafe1234", "probe.red"),
        "options": [],
        "events": [],
        "state": {},
    }
    monkeypatch.setattr("hive.tmux.list_panes_all", lambda: env["panes"])
    monkeypatch.setattr(
        "hive.adapters.claude_view.journal_signature", lambda: env["signature"]
    )
    monkeypatch.setattr(
        "hive.adapters.claude_view.view_for_pane", lambda _p, **kw: env["view"]
    )
    monkeypatch.setattr(
        "hive.adapters.claude_bg.job_id_for_pane", lambda _p: "cafe1234"
    )
    monkeypatch.setattr(
        "hive.tmux.set_pane_option",
        lambda pane, key, value: env["options"].append((pane, key, value)),
    )
    monkeypatch.setattr(
        "hive.notify_debug.emit",
        lambda ws, event, **fields: env["events"].append((event, fields)),
    )
    return env


def _run(env):
    hived._claude_view_tick(
        workspace="/tmp/ws", team="probe", members=MEMBERS, state=env["state"]
    )


def test_pane_on_its_own_member_carries_no_drift_label(tick):
    _run(tick)
    assert tick["options"] == [("%1", "hive-view", "")]
    assert tick["events"] == []


def test_switching_to_another_member_labels_the_border_and_logs_it(tick):
    tick["view"] = PaneView("likely", "member_view", "beef5678", "comb.blue")

    _run(tick)

    assert tick["options"] == [("%1", "hive-view", "comb.blue")]
    event, fields = tick["events"][0]
    assert event == "claude.view.foreign_member"
    assert (fields["viewing"], fields["otherTeam"]) == ("comb.blue", True)


def test_a_foreign_session_labels_the_border_without_an_event(tick):
    tick["view"] = PaneView("likely", "foreign", title="someone-elses-job")

    _run(tick)

    assert tick["options"] == [("%1", "hive-view", "someone-elses-job")]
    assert tick["events"] == []


def test_unchanged_signals_cost_nothing(tick):
    _run(tick)
    tick["options"].clear()

    _run(tick)  # same journal entries, same titles

    assert tick["options"] == []


def test_a_journal_change_re_probes_and_updates_the_label(tick):
    # Went to another member's session, then back to the panel list.
    tick["view"] = PaneView("likely", "member_view", "beef5678", "comb.blue")
    _run(tick)
    tick["options"].clear()
    tick["signature"] = ("two.json",)
    tick["view"] = PaneView("certain", "list_view")

    _run(tick)

    assert tick["options"] == [("%1", "hive-view", "")]


def test_a_title_change_alone_re_probes(tick):
    _run(tick)
    tick["options"].clear()
    tick["panes"] = [_pane(title="comb.blue")]
    tick["view"] = PaneView("likely", "member_view", "beef5678", "comb.blue")

    _run(tick)

    assert tick["options"] == [("%1", "hive-view", "comb.blue")]


def test_non_claude_members_are_left_alone(tick):
    tick["panes"] = [_pane(cli="codex")]

    _run(tick)

    assert tick["options"] == []


def test_an_empty_pane_listing_is_a_tmux_failure(tick):
    tick["panes"] = []

    _run(tick)

    assert tick["options"] == []
    assert tick["state"] == {}


# --- job names ------------------------------------------------------------


def _engine(job_id, name):
    from hive.adapters.claude_bg import EngineSession

    return EngineSession(
        pid=1, job_id=job_id, session_id="s", socket_path="/tmp/s", cwd="/repo",
        status="idle", waiting_for="", status_updated_at=0.0, name=name,
    )


def _name_wire(monkeypatch, *, jobs, engines):
    """jobs: pane -> job id. engines: job id -> engine (or None)."""
    from hive.adapters import claude_bg

    started = []
    monkeypatch.setattr(claude_bg, "job_id_for_pane", lambda pane: jobs.get(pane))
    monkeypatch.setattr(claude_bg, "engine_session_for_job", lambda job: engines.get(job))
    monkeypatch.setattr(
        hived.threading if hasattr(hived, "threading") else __import__("threading"),
        "Thread",
        lambda target, args, daemon: type(
            "T", (), {"start": lambda self: started.append((target, args))}
        )(),
    )
    return started


def test_a_placeholder_named_member_job_is_renamed_once(monkeypatch):
    """A pane adopted into a team (duo/squad/resume) was minted before it
    carried tags, so its job keeps `hive-<pane>`."""
    from hive.adapters import claude_bg

    started = _name_wire(
        monkeypatch,
        jobs={"%183": "485865b2"},
        engines={"485865b2": _engine("485865b2", "hive-183")},
    )
    state = {}
    members = {"worker": {"pane": "%183", "cli": "claude"}}

    hived._claude_name_tick(members=members, team="honey", state=state)
    hived._claude_name_tick(members=members, team="honey", state=state)

    assert len(started) == 1
    target, args = started[0]
    assert target is claude_bg.ensure_job_named
    assert args == ("485865b2", "honey.worker")


def test_an_already_named_job_is_left_alone(monkeypatch):
    started = _name_wire(
        monkeypatch,
        jobs={"%183": "485865b2"},
        engines={"485865b2": _engine("485865b2", "honey.worker")},
    )

    hived._claude_name_tick(
        members={"worker": {"pane": "%183", "cli": "claude"}}, team="honey", state={}
    )

    assert started == []


def test_an_asleep_engine_is_retried_on_a_later_tick(monkeypatch):
    """No entry means parked or gone — not a job that needs no rename."""
    started = _name_wire(monkeypatch, jobs={"%183": "485865b2"}, engines={})
    state = {}
    members = {"worker": {"pane": "%183", "cli": "claude"}}

    hived._claude_name_tick(members=members, team="honey", state=state)
    assert state.get("named", set()) == set()

    _name_wire(monkeypatch, jobs={"%183": "485865b2"},
               engines={"485865b2": _engine("485865b2", "hive-183")})
    hived._claude_name_tick(members=members, team="honey", state=state)
    assert state["named"] == {"485865b2"}


def test_non_claude_members_are_not_renamed(monkeypatch):
    started = _name_wire(monkeypatch, jobs={"%184": "job"}, engines={})

    hived._claude_name_tick(
        members={"validator": {"pane": "%184", "cli": "grok"}}, team="honey", state={}
    )

    assert started == []
