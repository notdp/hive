"""Tests for `_resolve_squad_worker_config` — per-role CLI + model for squad workers.

CLI precedence: `@hive-squad-worker` window tag > `roles.worker.cli` >
legacy `squad.duoWorker` > orch's CLI.
Model: `roles.worker.model` applies regardless of CLI source.
"""

import hive.cli as cli_mod


def _mock(monkeypatch, *, window_tag="", config="", orch_cli="claude",
          role_cli="", role_model=""):
    monkeypatch.setattr(
        cli_mod.tmux,
        "get_window_option",
        lambda _w, k: window_tag if k == "hive-squad-worker" else "",
    )

    settings_map = {}
    if config:
        settings_map["squad.duoWorker"] = config
    if role_cli:
        settings_map["roles.worker.cli"] = role_cli
    if role_model:
        settings_map["roles.worker.model"] = role_model
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda k, d=None: settings_map.get(k, d),
    )

    monkeypatch.setattr(
        cli_mod.tmux,
        "get_pane_option",
        lambda _p, k: orch_cli if k == "hive-cli" else "",
    )


def test_defaults_to_orch_family(monkeypatch):
    _mock(monkeypatch, orch_cli="codex")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("codex", "")


def test_window_tag_overrides_all(monkeypatch):
    _mock(monkeypatch, window_tag="codex", role_cli="droid", config="droid",
          orch_cli="claude", role_model="opus")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("codex", "opus")


def test_role_config_cli_overrides_legacy_and_orch(monkeypatch):
    _mock(monkeypatch, role_cli="droid", config="codex", orch_cli="claude")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("droid", "")


def test_legacy_config_still_works(monkeypatch):
    _mock(monkeypatch, config="codex", orch_cli="claude")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("codex", "")


def test_tag_beats_role_config(monkeypatch):
    _mock(monkeypatch, window_tag="droid", role_cli="codex", orch_cli="claude")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("droid", "")


def test_role_model_applies_with_any_cli_source(monkeypatch):
    _mock(monkeypatch, orch_cli="claude", role_model="opus")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("claude", "opus")


def test_role_model_applies_with_legacy_cli(monkeypatch):
    _mock(monkeypatch, config="codex", role_model="o3")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("codex", "o3")


def test_model_only_config(monkeypatch):
    _mock(monkeypatch, orch_cli="claude", role_model="opus")
    cli, model = cli_mod._resolve_squad_worker_config("%1", "dev:1")
    assert cli == "claude"
    assert model == "opus"


def test_ignores_legacy_crew_config_key(monkeypatch):
    monkeypatch.setattr(cli_mod.tmux, "get_window_option", lambda _w, _k: "")
    legacy = {"crew.cellWorker": "codex"}
    monkeypatch.setattr("hive.settings.get_setting", lambda k, d=None: legacy.get(k, d))
    monkeypatch.setattr(cli_mod.tmux, "get_pane_option", lambda _p, k: "claude" if k == "hive-cli" else "")
    assert cli_mod._resolve_squad_worker_config("%1", "dev:1") == ("claude", "")
