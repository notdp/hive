"""Tests for the squad-instance name pool (src/hive/squad_names.py).

The pool is the public namespace scheme that replaced the legacy fixed
``squad.*`` naming — each live squad picks a distinct name so qualified-name
lookup never hits an ambiguous match.
"""

import pytest

from hive import squad_names
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


@pytest.mark.parametrize("name", list(squad_names.SQUAD_NAME_POOL))
def test_validate_accepts_pool_names(name):
    ok, reason = squad_names.validate_name(name)
    assert ok, reason


@pytest.mark.parametrize(
    "name,reason_fragment",
    [
        ("", "empty"),
        ("squad", "reserved"),
        ("Peaky", "lowercase"),           # uppercase rejected
        ("peaky!", "lowercase"),          # punctuation rejected
        ("9crew", "lowercase"),           # leading digit rejected
        ("peaky_squad", "lowercase"),      # underscore rejected
        ("peakypeakypeakypeaky", "lowercase"),  # >16 chars
        ("-peaky", "lowercase"),          # leading dash rejected
    ],
)
def test_validate_rejects_invalid_names(name, reason_fragment):
    ok, reason = squad_names.validate_name(name)
    assert not ok
    assert reason_fragment in reason.lower()


# --- pick_available_name ---


def test_pick_returns_first_pool_name_when_nothing_claimed(monkeypatch):
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: [])
    assert squad_names.pick_available_name() == squad_names.SQUAD_NAME_POOL[0]


def test_pick_skips_claimed_pool_names(monkeypatch):
    claimed = [_pane("peaky"), _pane("krays", "%2")]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: claimed)
    # First two pool entries (peaky, krays) are taken → third (crips) wins.
    assert squad_names.pick_available_name() == "crips"


def test_pick_ignores_reserved_tokens_and_empty_groups(monkeypatch):
    # Bare "crew" (pre-rename legacy) and bare "squad" are reserved tokens,
    # never instance names — neither counts against the pool. Empty groups
    # (daily agent panes, shells) also ignored.
    panes = [
        _pane("crew"),        # pre-rename legacy literal, ignored
        _pane("squad", "%9"), # reserved topology word, ignored
        _pane(""),           # no tag, ignored
        _pane("   "),        # whitespace-only, ignored
        _pane("peer"),       # unrelated peer-group tag
    ]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    # None of the pool names are actually claimed → first pool entry wins.
    assert squad_names.pick_available_name() == squad_names.SQUAD_NAME_POOL[0]


def test_pick_falls_back_when_pool_exhausted(monkeypatch):
    # Every pool name claimed → fallback kicks in.
    panes = [_pane(name, f"%{i}") for i, name in enumerate(squad_names.SQUAD_NAME_POOL)]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    assert squad_names.pick_available_name("@42") == "squad-42"


def test_pick_fallback_strips_leading_at_sign(monkeypatch):
    panes = [_pane(name, f"%{i}") for i, name in enumerate(squad_names.SQUAD_NAME_POOL)]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    # Tmux window ids come as "@7"; the "@" must be stripped so the
    # fallback name is a valid identifier.
    assert squad_names.pick_available_name("@7") == "squad-7"


def test_pick_fallback_empty_suffix_defaults_to_zero(monkeypatch):
    panes = [_pane(name, f"%{i}") for i, name in enumerate(squad_names.SQUAD_NAME_POOL)]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    assert squad_names.pick_available_name("") == "squad-0"


def test_pick_fallback_disambiguates_when_same_suffix_already_taken(monkeypatch):
    # All pool taken + a prior squad already claimed "squad-7" → next caller
    # with the same suffix must not collide.
    panes = [_pane(name, f"%{i}") for i, name in enumerate(squad_names.SQUAD_NAME_POOL)]
    panes.append(_pane("squad-7", "%99"))
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    assert squad_names.pick_available_name("@7") == "squad-7-1"


def test_claimed_names_returns_distinct_groups(monkeypatch):
    panes = [
        _pane("peaky"),
        _pane("peaky", "%2"),     # duplicate, dedup in the set
        _pane("shelby", "%3"),
        _pane("", "%4"),
        _pane("squad", "%5"),      # reserved token filtered
        _pane("crew", "%6"),       # pre-rename legacy filtered
    ]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    assert squad_names.claimed_names() == {"peaky", "shelby"}


def test_validate_rejects_reserved_tokens():
    for reserved in ("squad", "crew"):
        ok, reason = squad_names.validate_name(reserved)
        assert not ok and "reserved" in reason
