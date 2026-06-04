"""Tests for `_resolve_crew_worker_cli` — the one crew-cell worker knob.

Default = orch's family (worker same-family as orch; the anti-family review
seat goes to the validator). Override precedence: `@hive-crew-worker` window
tag (set by `crew init --worker`) > global `crew.cellWorker` config > orch's
CLI.
"""

import hive.cli as cli_mod


def _mock(monkeypatch, *, window_tag="", config="", orch_cli="claude"):
    monkeypatch.setattr(
        cli_mod.tmux,
        "get_window_option",
        lambda _w, k: window_tag if k == "hive-crew-worker" else "",
    )
    monkeypatch.setattr(
        "hive.settings.get_setting",
        lambda k, d: config if (k == "crew.cellWorker" and config) else d,
    )
    monkeypatch.setattr(
        cli_mod.tmux,
        "get_pane_option",
        lambda _p, k: orch_cli if k == "hive-cli" else "",
    )


def test_crew_worker_cli_defaults_to_orch_family(monkeypatch):
    _mock(monkeypatch, orch_cli="codex")
    assert cli_mod._resolve_crew_worker_cli("%1", "dev:1") == "codex"


def test_crew_worker_cli_window_tag_overrides_orch(monkeypatch):
    _mock(monkeypatch, window_tag="codex", orch_cli="claude")
    assert cli_mod._resolve_crew_worker_cli("%1", "dev:1") == "codex"


def test_crew_worker_cli_config_default_when_no_tag(monkeypatch):
    _mock(monkeypatch, config="codex", orch_cli="claude")
    assert cli_mod._resolve_crew_worker_cli("%1", "dev:1") == "codex"


def test_crew_worker_cli_tag_beats_config(monkeypatch):
    _mock(monkeypatch, window_tag="droid", config="codex", orch_cli="claude")
    assert cli_mod._resolve_crew_worker_cli("%1", "dev:1") == "droid"
