"""Real-tmux rendering of the hive pane-border format.

The claude member border contract: the pane is an attach *viewer*, so it
shows the bare member name while the view matches the member and
"name -> what is really on screen" once the human switched the viewer
elsewhere. The switch is detected by the sidecar's view probe and handed to
tmux as the pane's `@hive-view` option; tmux itself evaluates the format, so
the assertions run the real renderer via `display-message -p`.
"""
import shutil
import uuid

import pytest

from hive import tmux as hive_tmux
from tests.e2e._helpers import run_tmux

pytestmark = pytest.mark.skipif(
    shutil.which("tmux") is None, reason="tmux is required for e2e tests"
)


@pytest.fixture
def border_pane():
    session = f"hive-e2e-{uuid.uuid4().hex[:8]}"
    pane = run_tmux(
        ["new-session", "-d", "-s", session, "-x", "80", "-y", "20", "-P", "-F", "#{pane_id}"]
    ).stdout.strip()
    try:
        yield pane
    finally:
        run_tmux(["kill-session", "-t", session])


def _render(pane: str) -> str:
    return run_tmux(
        ["display-message", "-p", "-t", pane, hive_tmux._HIVE_PANE_BORDER_FORMAT]
    ).stdout.rstrip("\n")


def _set(pane: str, key: str, value: str) -> None:
    run_tmux(["set-option", "-p", "-t", pane, key, value])


def test_border_follows_the_viewed_session(border_pane):
    _set(border_pane, "@hive-agent", "red")
    _set(border_pane, "@hive-team", "probe")
    _set(border_pane, "@hive-cli", "claude")
    # A drifted terminal title never speaks for itself: the probe does.
    run_tmux(["select-pane", "-t", border_pane, "-T", "whatever the TUI wrote"])

    # On its own member (or nothing identifiable on screen): the member's
    # full name. `red` alone says nothing about which team's red it is when
    # several teams are on screen.
    _set(border_pane, "@hive-view", "")
    assert _render(border_pane) == " probe.red "

    # Viewer switched to another member: dual display, both sides named.
    _set(border_pane, "@hive-view", "comb.blue")
    assert _render(border_pane) == " probe.red#[fg=colour220] -> comb.blue#[default] "

    # Notify marker composes with the drift suffix.
    _set(border_pane, "@hive-notify-active", "1")
    assert _render(border_pane).startswith(" #[fg=colour220]#[bold][!] #[default]probe.red")


def test_border_untagged_pane_falls_back_to_pane_title(border_pane):
    run_tmux(["select-pane", "-t", border_pane, "-T", "plain shell"])
    assert _render(border_pane) == " plain shell "


def test_border_without_a_team_tag_shows_the_bare_agent(border_pane):
    """A pane tagged as an agent but with no team is still labelled, not
    dropped to its terminal title."""
    _set(border_pane, "@hive-agent", "red")
    run_tmux(["select-pane", "-t", border_pane, "-T", "whatever the TUI wrote"])
    assert _render(border_pane) == " red "
