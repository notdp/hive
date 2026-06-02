"""Tests for the gang-instance name pool (src/hive/gang_names.py).

The pool is the public namespace scheme that replaced the legacy fixed
``gang.*`` naming — each live gang picks a distinct name so qualified-name
lookup never hits an ambiguous match.
"""

import pytest

from hive import gang_names
from hive.tmux import PaneInfo


def _pane(group: str, pane_id: str = "%1") -> PaneInfo:
    return PaneInfo(
        pane_id=pane_id,
        title="",
        command="",
        role="agent",
        agent="",
        team="",
        cli="",
        group=group,
    )


# --- validate_name ---


@pytest.mark.parametrize("name", list(gang_names.GANG_NAME_POOL))
def test_validate_accepts_pool_names(name):
    ok, reason = gang_names.validate_name(name)
    assert ok, reason


@pytest.mark.parametrize(
    "name,reason_fragment",
    [
        ("", "empty"),
        ("gang", "reserved"),
        ("Peaky", "lowercase"),           # uppercase rejected
        ("peaky!", "lowercase"),          # punctuation rejected
        ("9gang", "lowercase"),           # leading digit rejected
        ("peaky_gang", "lowercase"),      # underscore rejected
        ("peakypeakypeakypeaky", "lowercase"),  # >16 chars
        ("-peaky", "lowercase"),          # leading dash rejected
    ],
)
def test_validate_rejects_invalid_names(name, reason_fragment):
    ok, reason = gang_names.validate_name(name)
    assert not ok
    assert reason_fragment in reason.lower()


# --- pick_available_name ---


def test_pick_returns_first_pool_name_when_nothing_claimed(monkeypatch):
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: [])
    assert gang_names.pick_available_name() == gang_names.GANG_NAME_POOL[0]


def test_pick_skips_claimed_pool_names(monkeypatch):
    claimed = [_pane("peaky"), _pane("krays", "%2")]
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: claimed)
    # First two pool entries (peaky, krays) are taken → third (crips) wins.
    assert gang_names.pick_available_name() == "crips"


def test_pick_ignores_legacy_gang_token_and_empty_groups(monkeypatch):
    # @hive-group="gang" is the pre-migration literal; must NOT be counted
    # against the pool. Empty groups (daily agent panes, shells) also ignored.
    panes = [
        _pane("gang"),       # legacy, ignored
        _pane(""),           # no tag, ignored
        _pane("   "),        # whitespace-only, ignored
        _pane("peer"),       # unrelated peer-group tag
    ]
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: panes)
    # None of the pool names are actually claimed → first pool entry wins.
    assert gang_names.pick_available_name() == gang_names.GANG_NAME_POOL[0]


def test_pick_falls_back_when_pool_exhausted(monkeypatch):
    # Every pool name claimed → fallback kicks in.
    panes = [_pane(name, f"%{i}") for i, name in enumerate(gang_names.GANG_NAME_POOL)]
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: panes)
    assert gang_names.pick_available_name("@42") == "gang-42"


def test_pick_fallback_strips_leading_at_sign(monkeypatch):
    panes = [_pane(name, f"%{i}") for i, name in enumerate(gang_names.GANG_NAME_POOL)]
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: panes)
    # Tmux window ids come as "@7"; the "@" must be stripped so the
    # fallback name is a valid identifier.
    assert gang_names.pick_available_name("@7") == "gang-7"


def test_pick_fallback_empty_suffix_defaults_to_zero(monkeypatch):
    panes = [_pane(name, f"%{i}") for i, name in enumerate(gang_names.GANG_NAME_POOL)]
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: panes)
    assert gang_names.pick_available_name("") == "gang-0"


def test_pick_fallback_disambiguates_when_same_suffix_already_taken(monkeypatch):
    # All pool taken + a prior gang already claimed "gang-7" → next caller
    # with the same suffix must not collide.
    panes = [_pane(name, f"%{i}") for i, name in enumerate(gang_names.GANG_NAME_POOL)]
    panes.append(_pane("gang-7", "%99"))
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: panes)
    assert gang_names.pick_available_name("@7") == "gang-7-1"


def test_claimed_names_returns_distinct_groups(monkeypatch):
    panes = [
        _pane("peaky"),
        _pane("peaky", "%2"),     # duplicate, dedup in the set
        _pane("shelby", "%3"),
        _pane("", "%4"),
        _pane("gang", "%5"),      # legacy literal filtered
    ]
    monkeypatch.setattr(gang_names.tmux, "list_panes_all", lambda: panes)
    assert gang_names.claimed_names() == {"peaky", "shelby"}
