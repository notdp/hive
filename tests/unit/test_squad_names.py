"""Tests for the squad-instance name pool (src/hive/squad_names.py).

The pool is the public namespace scheme that replaced the legacy fixed
``squad.*`` naming — each live squad picks a distinct name so qualified-name
lookup never hits an ambiguous match.
"""

import pytest

from hive import squad_names
from hive.tmux import PaneInfo


def _pane(group: str, pane_id: str = "%1", agent: str = "") -> PaneInfo:
    return PaneInfo(
        pane_id=pane_id,
        title="",
        command="",
        role="agent",
        agent=agent,
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


# --- validate_feature_id ---


@pytest.mark.parametrize(
    "feature_id",
    [
        "contract-usd-amount-words",
        "pr-window-anchor",
        "fix-2fa",
        "auth",
        "v2-api-keys",
    ],
)
def test_validate_feature_id_accepts_semantic_kebab(feature_id):
    ok, reason = squad_names.validate_feature_id(feature_id)
    assert ok, reason


@pytest.mark.parametrize(
    "feature_id,reason_fragment",
    [
        ("", "empty"),
        ("F2-03_04", "kebab-case"),    # the live-run mess: uppercase + underscores
        ("f2-03_04", "kebab-case"),    # underscores
        ("F1", "kebab-case"),          # uppercase ordinal
        ("Auth-Fix", "kebab-case"),    # uppercase
        ("auth.fix", "kebab-case"),    # punctuation
        ("2fa-fix", "kebab-case"),     # leading digit
        ("a-b-c-d-e", "kebab-case"),   # >4 segments
        ("f1", "step/sequence"),       # lowercase feature ordinal
        ("f2-03-04", "step/sequence"), # all-numeric segments
        ("auth-03", "step/sequence"),  # trailing step number
        ("contract-usd-amount-words-extra-long", "too long"),  # >32 chars
    ],
)
def test_validate_feature_id_rejects_step_ids(feature_id, reason_fragment):
    ok, reason = squad_names.validate_feature_id(feature_id)
    assert not ok
    assert reason_fragment in reason


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


def test_claimed_names_includes_qualified_agent_prefix(monkeypatch):
    """A pane with @hive-agent=krays.coco but no @hive-group still claims 'krays'."""
    panes = [
        _pane("peaky", agent="peaky.orch"),          # group + agent both claim
        _pane("", "%2", agent="krays.coco"),          # no group, agent claims
        _pane("", "%3", agent="worker"),              # unqualified, no claim
    ]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    assert squad_names.claimed_names() == {"peaky", "krays"}


def test_claimed_names_ignores_invalid_agent_prefix(monkeypatch):
    """Qualified agent prefixes that fail validate_name are not claimed."""
    panes = [
        _pane("", agent="9bad.worker"),      # leading digit → invalid
        _pane("", "%2", agent="squad.orch"), # reserved → invalid
        _pane("", "%3", agent=".worker"),    # empty prefix → no dot split
    ]
    monkeypatch.setattr(squad_names.tmux, "list_panes_all", lambda: panes)
    assert squad_names.claimed_names() == set()


def test_validate_rejects_reserved_tokens():
    for reserved in ("squad", "crew"):
        ok, reason = squad_names.validate_name(reserved)
        assert not ok and "reserved" in reason
