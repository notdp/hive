"""Tests for `resolve_role_config` — per-role CLI + model overrides."""

import pytest

from hive.settings import resolve_role_config, CONFIGURABLE_ROLES, APPLIED_ROLES


def _set_role(monkeypatch, role, *, cli=None, model=None):
    """Patch `hive.settings.get_setting` to return *cli*/*model* for the given role."""
    overrides = {}
    if cli is not None:
        overrides[f"roles.{role}.cli"] = cli
    if model is not None:
        overrides[f"roles.{role}.model"] = model
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: overrides.get(key, default),
    )


def test_returns_configured_cli_and_model(monkeypatch):
    _set_role(monkeypatch, "validator", cli="codex", model="o3")
    assert resolve_role_config("validator") == ("codex", "o3")


def test_unset_role_returns_empty(monkeypatch):
    _set_role(monkeypatch, "worker")
    assert resolve_role_config("worker") == ("", "")


def test_unknown_role_returns_empty(monkeypatch):
    _set_role(monkeypatch, "bogus", cli="claude", model="opus")
    assert resolve_role_config("bogus") == ("", "")


def test_invalid_cli_ignored(monkeypatch):
    _set_role(monkeypatch, "validator", cli="not-a-cli", model="o3")
    assert resolve_role_config("validator") == ("", "o3")


def test_non_string_cli_ignored(monkeypatch):
    _set_role(monkeypatch, "validator", cli=42, model="o3")
    assert resolve_role_config("validator") == ("", "o3")


def test_empty_model_ignored(monkeypatch):
    _set_role(monkeypatch, "worker", cli="claude", model="")
    assert resolve_role_config("worker") == ("claude", "")


def test_non_string_model_ignored(monkeypatch):
    _set_role(monkeypatch, "worker", cli="claude", model=True)
    assert resolve_role_config("worker") == ("claude", "")


def test_model_only_config(monkeypatch):
    _set_role(monkeypatch, "challenger", model="opus")
    assert resolve_role_config("challenger") == ("", "opus")


@pytest.mark.parametrize("role", sorted(CONFIGURABLE_ROLES))
def test_all_configurable_roles_accepted(monkeypatch, role):
    _set_role(monkeypatch, role, cli="claude", model="opus")
    assert resolve_role_config(role) == ("claude", "opus")


def test_applied_roles_subset_of_configurable():
    assert APPLIED_ROLES < CONFIGURABLE_ROLES
    assert "orch" not in APPLIED_ROLES
