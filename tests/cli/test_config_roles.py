"""Tests for `hive config roles` — JSON output and interactive picker."""

import json

import click
import pytest

from hive.cli import cli, _interactive_role_config
from hive.settings import APPLIED_ROLES, CONFIGURABLE_ROLES


# --- JSON mode (--json flag and non-TTY fallback) ---


def test_config_roles_json_flag_empty(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)
    result = runner.invoke(cli, ["config", "roles", "--json"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert set(data.keys()) == CONFIGURABLE_ROLES
    for role, info in data.items():
        assert info["cli"] == ""
        assert info["model"] == ""
        assert info["applied"] == (role in APPLIED_ROLES)


def test_config_roles_json_flag_with_overrides(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda key, default=None: {
            "roles.validator.cli": "codex",
            "roles.validator.model": "gpt-5.5",
            "roles.worker.model": "opus",
        }.get(key, default),
    )
    result = runner.invoke(cli, ["config", "roles", "--json"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert data["validator"]["cli"] == "codex"
    assert data["validator"]["model"] == "gpt-5.5"
    assert data["worker"]["model"] == "opus"
    assert data["worker"]["cli"] == ""
    assert data["orch"]["applied"] is False
    assert data["challenger"]["applied"] is True


def test_config_roles_non_tty_outputs_json(runner, configure_hive_home):
    """Non-TTY without --json emits JSON (compat), no prompt, no mutation."""
    configure_hive_home(tmux_inside=False)
    # CliRunner stdin is not a TTY, so the non-interactive branch fires.
    result = runner.invoke(cli, ["config", "roles"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert set(data.keys()) == CONFIGURABLE_ROLES


# --- Interactive picker (exercises _interactive_role_config directly) ---


def _run_interactive(monkeypatch, inputs, *, settings_store=None):
    """Run _interactive_role_config with simulated click.prompt inputs.

    Returns the settings_store dict reflecting all set/unset calls.
    """
    if settings_store is None:
        settings_store = {}
    input_iter = iter(inputs)

    original_prompt = click.prompt

    def fake_prompt(text, **kwargs):
        try:
            value = next(input_iter)
        except StopIteration:
            raise click.Abort()
        choice_type = kwargs.get("type")
        if isinstance(choice_type, click.Choice):
            if value.lower() not in [c.lower() for c in choice_type.choices]:
                raise ValueError(f"invalid choice: {value}")
        return value

    monkeypatch.setattr("click.prompt", fake_prompt)

    monkeypatch.setattr(
        "hive.settings.resolve_role_config",
        lambda role: (settings_store.get(f"roles.{role}.cli", ""),
                      settings_store.get(f"roles.{role}.model", "")),
    )
    monkeypatch.setattr(
        "hive.settings.set_setting",
        lambda key, value: settings_store.__setitem__(key, value),
    )

    _SENTINEL = object()
    def fake_unset(key):
        if key in settings_store:
            del settings_store[key]
            return True
        return False

    monkeypatch.setattr("hive.settings.unset_setting", fake_unset)

    _interactive_role_config()
    return settings_store


def test_interactive_select_role_cli_and_suggested_model(monkeypatch):
    """Pick validator → codex → gpt-5.5 (suggestion 1)."""
    store = _run_interactive(monkeypatch, [
        "validator",    # role
        "codex",        # CLI
        "1",            # model: first suggestion (gpt-5.5)
        "done",         # exit
    ])
    assert store["roles.validator.cli"] == "codex"
    assert store["roles.validator.model"] == "gpt-5.5"


def test_interactive_custom_model(monkeypatch):
    """Pick worker → claude → custom model value."""
    store = _run_interactive(monkeypatch, [
        "worker",       # role
        "claude",       # CLI
        "19",           # custom value option (after 18 claude suggestions)
        "my-custom-model",
        "done",
    ])
    assert store["roles.worker.cli"] == "claude"
    assert store["roles.worker.model"] == "my-custom-model"


def test_interactive_keep_preserves_existing(monkeypatch):
    """'keep' leaves both CLI and model untouched."""
    initial = {
        "roles.validator.cli": "codex",
        "roles.validator.model": "gpt-5.5",
    }
    store = _run_interactive(monkeypatch, [
        "validator",
        "keep",         # keep CLI
        # model suggestion list for codex shown (existing effective_cli)
        str(4 + 2),     # keep option = len(codex suggestions) + 2 = 6
        "done",
    ], settings_store=dict(initial))
    assert store["roles.validator.cli"] == "codex"
    assert store["roles.validator.model"] == "gpt-5.5"


def test_interactive_clear_deletes_key(monkeypatch):
    """'clear' removes the key via unset, not empty string."""
    initial = {
        "roles.validator.cli": "codex",
        "roles.validator.model": "gpt-5.5",
    }
    store = _run_interactive(monkeypatch, [
        "validator",
        "clear",        # clear CLI
        # no picker CLI → custom/keep/clear prompt
        "clear",        # clear model
        "done",
    ], settings_store=dict(initial))
    assert "roles.validator.cli" not in store
    assert "roles.validator.model" not in store


def test_interactive_clear_cli_leaves_model_intact(monkeypatch):
    """Clearing CLI does not touch model."""
    initial = {
        "roles.worker.cli": "claude",
        "roles.worker.model": "opus",
    }
    store = _run_interactive(monkeypatch, [
        "worker",
        "clear",        # clear CLI
        "keep",         # keep model (no CLI → custom/keep/clear)
        "done",
    ], settings_store=dict(initial))
    assert "roles.worker.cli" not in store
    assert store["roles.worker.model"] == "opus"


def test_interactive_clear_model_leaves_cli_intact(monkeypatch):
    """Clearing model does not touch CLI."""
    initial = {
        "roles.worker.cli": "claude",
        "roles.worker.model": "opus",
    }
    store = _run_interactive(monkeypatch, [
        "worker",
        "keep",         # keep CLI
        # claude suggestions shown; clear = len(suggestions) + 3
        str(18 + 3),    # clear option = 21
        "done",
    ], settings_store=dict(initial))
    assert store["roles.worker.cli"] == "claude"
    assert "roles.worker.model" not in store


def test_interactive_orch_not_in_choices(monkeypatch):
    """orch is displayed as stored-only but not offered in the picker."""
    with pytest.raises(ValueError, match="invalid choice"):
        _run_interactive(monkeypatch, ["orch"])


def test_interactive_no_cli_shows_no_suggestions(monkeypatch):
    """When no CLI is set and user picks 'keep', model prompt is custom/keep/clear only."""
    store = _run_interactive(monkeypatch, [
        "challenger",
        "keep",         # no current CLI → effective_cli=""
        "custom",       # custom/keep/clear prompt (no suggestion list)
        "my-model",
        "done",
    ])
    assert store["roles.challenger.model"] == "my-model"
    assert "roles.challenger.cli" not in store


def test_interactive_eof_aborts_no_partial_mutation(monkeypatch):
    """EOF after CLI choice but before model choice: store must be empty."""
    store = {}
    with pytest.raises(click.Abort):
        _run_interactive(monkeypatch, [
            "validator",
            "codex",
            # EOF here — no model choice
        ], settings_store=store)
    assert store == {}, f"partial mutation on abort: {store}"


def test_interactive_eof_preserves_existing_config(monkeypatch):
    """EOF during role config must not mutate pre-existing values."""
    initial = {
        "roles.validator.cli": "codex",
        "roles.validator.model": "gpt-5.5",
    }
    store = dict(initial)
    with pytest.raises(click.Abort):
        _run_interactive(monkeypatch, [
            "validator",
            "clear",        # would clear CLI
            # EOF here — no model choice
        ], settings_store=store)
    assert store == initial, f"existing config mutated on abort: {store}"


def test_interactive_multiple_roles(monkeypatch):
    """Configure two roles in one session."""
    store = _run_interactive(monkeypatch, [
        "worker",
        "claude",
        "1",            # opus
        "validator",
        "codex",
        "1",            # gpt-5.5
        "done",
    ])
    assert store["roles.worker.cli"] == "claude"
    assert store["roles.worker.model"] == "opus"
    assert store["roles.validator.cli"] == "codex"
    assert store["roles.validator.model"] == "gpt-5.5"
