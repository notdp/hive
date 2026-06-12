"""Tests for `hive config roles` — JSON output and interactive picker."""

import json

import click
import pytest

from hive.cli import cli, _interactive_role_config, _collect_role_choices, _apply_role_action
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
    result = runner.invoke(cli, ["config", "roles"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.output)
    assert set(data.keys()) == CONFIGURABLE_ROLES


# --- Interactive picker tests ---
#
# _term_menu requires a real terminal; tests mock it to return menu indices.
# _collect_role_choices is tested directly to cover the choice→action logic.
# _interactive_role_config is tested with mocked _term_menu + settings.


def _mock_settings(monkeypatch, store):
    """Patch settings to use an in-memory store dict."""
    monkeypatch.setattr(
        "hive.settings.resolve_role_config",
        lambda role: (store.get(f"roles.{role}.cli", ""),
                      store.get(f"roles.{role}.model", "")),
    )
    monkeypatch.setattr(
        "hive.settings.set_setting",
        lambda key, value: store.__setitem__(key, value),
    )
    monkeypatch.setattr(
        "hive.settings.unset_setting",
        lambda key: store.pop(key, None) is not None,
    )


def _mock_menus(monkeypatch, indices):
    """Patch _term_menu to return indices from a list sequentially."""
    it = iter(indices)
    monkeypatch.setattr("hive.cli._term_menu", lambda entries, title, **kw: next(it))


def _run_interactive(monkeypatch, menu_indices, *, settings_store=None,
                     custom_inputs=None):
    """Run _interactive_role_config with mocked _term_menu indices.

    menu_indices: sequence of int|None for each _term_menu call.
    custom_inputs: optional sequence of strings for click.prompt (custom model).
    """
    if settings_store is None:
        settings_store = {}
    _mock_settings(monkeypatch, settings_store)
    _mock_menus(monkeypatch, menu_indices)
    if custom_inputs:
        it = iter(custom_inputs)
        monkeypatch.setattr("click.prompt", lambda text, **kw: next(it))
    _interactive_role_config()
    return settings_store


def test_interactive_select_role_cli_and_model(monkeypatch):
    """Pick validator → codex → gpt-5.5 (first codex suggestion)."""
    store = _run_interactive(monkeypatch, [
        1,    # role: validator (challenger=0, validator=1, worker=2, done=3)
        1,    # CLI: codex (claude=0, codex=1, droid=2, keep=3, clear=4)
        0,    # model: first codex suggestion (gpt-5.5)
        3,    # role: done
    ])
    assert store["roles.validator.cli"] == "codex"
    assert store["roles.validator.model"] == "gpt-5.5"


def test_interactive_custom_model(monkeypatch):
    """Pick worker → claude → custom model value."""
    from hive.agent_cli import MODEL_SUGGESTIONS
    n_claude = len(MODEL_SUGGESTIONS["claude"])
    store = _run_interactive(monkeypatch, [
        2,          # role: worker
        0,          # CLI: claude
        n_claude,   # model: (custom) — right after suggestions
        3,          # role: done
    ], custom_inputs=["my-custom-model"])
    assert store["roles.worker.cli"] == "claude"
    assert store["roles.worker.model"] == "my-custom-model"


def test_interactive_keep_preserves_existing(monkeypatch):
    """'keep' leaves both CLI and model untouched."""
    initial = {
        "roles.validator.cli": "codex",
        "roles.validator.model": "gpt-5.5",
    }
    from hive.agent_cli import MODEL_SUGGESTIONS
    n_codex = len(MODEL_SUGGESTIONS["codex"])
    store = _run_interactive(monkeypatch, [
        1,              # role: validator
        3,              # CLI: (keep)
        n_codex + 1,    # model: (keep) — custom=n, keep=n+1, clear=n+2
        3,              # role: done
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
        1,    # role: validator
        4,    # CLI: (clear)
        # no suggestions (no effective CLI) → (custom)=0, (keep)=1, (clear)=2
        2,    # model: (clear)
        3,    # role: done
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
        2,    # role: worker
        4,    # CLI: (clear)
        1,    # model: (keep) — no suggestions → custom=0, keep=1, clear=2
        3,    # role: done
    ], settings_store=dict(initial))
    assert "roles.worker.cli" not in store
    assert store["roles.worker.model"] == "opus"


def test_interactive_clear_model_leaves_cli_intact(monkeypatch):
    """Clearing model does not touch CLI."""
    initial = {
        "roles.worker.cli": "claude",
        "roles.worker.model": "opus",
    }
    from hive.agent_cli import MODEL_SUGGESTIONS
    n_claude = len(MODEL_SUGGESTIONS["claude"])
    store = _run_interactive(monkeypatch, [
        2,              # role: worker
        3,              # CLI: (keep)
        n_claude + 2,   # model: (clear) — custom=n, keep=n+1, clear=n+2
        3,              # role: done
    ], settings_store=dict(initial))
    assert store["roles.worker.cli"] == "claude"
    assert "roles.worker.model" not in store


def test_interactive_orch_not_in_role_list(monkeypatch):
    """orch is not offered in the interactive role list."""
    from hive.settings import APPLIED_ROLES
    applied = sorted(APPLIED_ROLES)
    entries = [*applied, "done"]
    assert "orch" not in entries


def test_interactive_escape_aborts_no_mutation(monkeypatch):
    """Escape (None index) at role menu → exit cleanly, no mutation."""
    store = {}
    _mock_settings(monkeypatch, store)
    _mock_menus(monkeypatch, [None])  # Escape at role menu
    _interactive_role_config()
    assert store == {}


def test_interactive_escape_at_cli_aborts_no_mutation(monkeypatch):
    """Escape at CLI menu → Abort, no partial mutation."""
    store = {}
    _mock_settings(monkeypatch, store)
    _mock_menus(monkeypatch, [
        1,      # role: validator
        None,   # Escape at CLI menu
    ])
    with pytest.raises(click.Abort):
        _interactive_role_config()
    assert store == {}


def test_interactive_escape_at_model_aborts_no_mutation(monkeypatch):
    """Escape at model menu → Abort, no partial mutation even after CLI choice."""
    initial = {
        "roles.validator.cli": "codex",
        "roles.validator.model": "gpt-5.5",
    }
    store = dict(initial)
    _mock_settings(monkeypatch, store)
    _mock_menus(monkeypatch, [
        1,      # role: validator
        4,      # CLI: (clear) — would clear if completed
        None,   # Escape at model menu
    ])
    with pytest.raises(click.Abort):
        _interactive_role_config()
    assert store == initial


def test_interactive_multiple_roles(monkeypatch):
    """Configure two roles in one session."""
    store = _run_interactive(monkeypatch, [
        2,    # role: worker
        0,    # CLI: claude
        0,    # model: first claude suggestion (claude-fable-5)
        1,    # role: validator
        1,    # CLI: codex
        0,    # model: first codex suggestion (gpt-5.5)
        3,    # role: done
    ])
    assert store["roles.worker.cli"] == "claude"
    assert store["roles.worker.model"] == "claude-fable-5"
    assert store["roles.validator.cli"] == "codex"
    assert store["roles.validator.model"] == "gpt-5.5"


def test_interactive_cli_cursor_starts_at_current(monkeypatch):
    """CLI menu cursor should start on the current CLI, not index 0."""
    store = {"roles.validator.cli": "codex", "roles.validator.model": "gpt-5.5"}
    _mock_settings(monkeypatch, store)

    calls: list[dict] = []
    menu_indices = iter([1, 3, 4 + 1, 3])  # role=validator, CLI=keep, model=keep, done

    def tracking_menu(entries, title, **kw):
        calls.append({"entries": entries, "title": title, **kw})
        return next(menu_indices)

    monkeypatch.setattr("hive.cli._term_menu", tracking_menu)
    _interactive_role_config()

    cli_call = [c for c in calls if "CLI" in c["title"]][0]
    cli_names = [e.split("  ←")[0] for e in cli_call["entries"] if "←" not in e or True]
    codex_idx = next(i for i, e in enumerate(cli_call["entries"]) if e.startswith("codex"))
    assert cli_call["cursor_index"] == codex_idx


def test_interactive_model_cursor_starts_at_current(monkeypatch):
    """Model menu cursor should start on the current model in suggestions."""
    store = {"roles.validator.cli": "codex", "roles.validator.model": "gpt-5.4"}
    _mock_settings(monkeypatch, store)

    calls: list[dict] = []
    menu_indices = iter([1, 3, 0, 3])  # role=validator, CLI=keep, model=first, done

    def tracking_menu(entries, title, **kw):
        calls.append({"entries": entries, "title": title, **kw})
        return next(menu_indices)

    monkeypatch.setattr("hive.cli._term_menu", tracking_menu)
    _interactive_role_config()

    model_call = [c for c in calls if "Model" in c["title"]][0]
    from hive.agent_cli import MODEL_SUGGESTIONS
    expected_cursor = MODEL_SUGGESTIONS["codex"].index("gpt-5.4")
    assert model_call["cursor_index"] == expected_cursor


def test_interactive_model_cursor_custom_value_focuses_keep(monkeypatch):
    """When current model is custom (not in suggestions), cursor lands on (keep)."""
    store = {"roles.validator.cli": "codex", "roles.validator.model": "my-custom-thing"}
    _mock_settings(monkeypatch, store)

    calls: list[dict] = []
    menu_indices = iter([1, 3, 0, 3])  # role=validator, CLI=keep, model=first, done

    def tracking_menu(entries, title, **kw):
        calls.append({"entries": entries, "title": title, **kw})
        return next(menu_indices)

    monkeypatch.setattr("hive.cli._term_menu", tracking_menu)
    _interactive_role_config()

    model_call = [c for c in calls if "Model" in c["title"]][0]
    keep_idx = model_call["entries"].index("(keep)")
    assert model_call["cursor_index"] == keep_idx


def test_interactive_no_cli_shows_no_suggestions(monkeypatch):
    """When no CLI and user keeps, model prompt offers only custom/keep/clear."""
    store = _run_interactive(monkeypatch, [
        0,    # role: challenger
        3,    # CLI: (keep) — no current CLI
        0,    # model: (custom) — no suggestions → custom=0, keep=1, clear=2
        3,    # role: done
    ], custom_inputs=["my-model"])
    assert store["roles.challenger.model"] == "my-model"
    assert "roles.challenger.cli" not in store
