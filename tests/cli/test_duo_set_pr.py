"""Tests for `hive duo set-pr`: PR-number window labeling + native display.

set-pr writes window-local state only — the `@hive-pr` data option plus
per-window `window-status-format` / `window-status-current-format` derived
from the *global* values (the index position renders `PR<n>`, user styling
preserved). It also renames the window to the feature: explicit TITLE wins,
otherwise the cwd's hive feature branch; with neither the name is left
alone. It must never touch the window index or write global options; every
failure path must leave all options unwritten.
"""

import json

import pytest

from hive.cli import cli

pytestmark = pytest.mark.cli

DERIVED_DEFAULT = "#{?#{@hive-pr},PR#{@hive-pr},#I}:#W#{?window_flags,#{window_flags}, }"


@pytest.fixture(autouse=True)
def no_feature_cwd(monkeypatch):
    """Pin title derivation to "no feature": the test process itself runs on
    some real git branch, which must never leak into assertions. Rename-path
    tests override this explicitly."""


def _capture_renames(monkeypatch) -> list:
    """Swap the conftest no-op rename_window for a recorder — must run *after*
    configure_hive_home(), which installs its own patch."""
    calls: list = []
    monkeypatch.setattr(
        "hive.cli.tmux.rename_window", lambda *a, **k: calls.append((a, k))
    )
    return calls


def _bind_team(window: str = "dev:0", team: str = "t-duo") -> None:
    from hive import cli as cli_mod

    cli_mod.tmux.set_window_option(window, "@hive-team", team)


def _window_option(key: str, window: str = "dev:0"):
    from hive import cli as cli_mod

    return cli_mod.tmux.get_window_option(window, key)


def _assert_nothing_written() -> None:
    assert _window_option("hive-pr") is None
    assert _window_option("window-status-format") is None
    assert _window_option("window-status-current-format") is None


def test_set_pr_stamps_option_and_derives_display_json(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _bind_team()
    renames = _capture_renames(monkeypatch)

    result = runner.invoke(cli, ["duo", "set-pr", "87", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {
        "window": "dev:0",
        "pr": 87,
        "display": {
            "window-status-format": "derived",
            "window-status-current-format": "derived",
        },
    }
    assert _window_option("hive-pr") == "87"
    # Window-local display derived from the mocked global default — the
    # index token is swapped for the PR conditional, nothing else changes.
    assert _window_option("window-status-format") == DERIVED_DEFAULT
    assert _window_option("window-status-current-format") == DERIVED_DEFAULT
    # No explicit TITLE and no hive feature branch in cwd → name left alone.
    assert renames == []


def test_set_pr_never_renames_the_window(runner, configure_hive_home, monkeypatch):
    """The window name is the team's identity; stamping a PR number labels the
    window status format instead of overwriting that name."""
    configure_hive_home()
    _bind_team()
    renames = _capture_renames(monkeypatch)

    result = runner.invoke(cli, ["duo", "set-pr", "87", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert renames == []
    assert "title" not in payload
    assert _window_option("hive-pr") == "87"
    assert _window_option("window-status-format") == DERIVED_DEFAULT


def test_set_pr_default_output_is_json(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()

    result = runner.invoke(cli, ["duo", "set-pr", "87"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["pr"] == 87
    assert payload["display"]


def test_set_pr_plain_output_is_human_line(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()

    result = runner.invoke(cli, ["duo", "set-pr", "87", "--plain"])

    assert result.exit_code == 0, result.output
    assert "@hive-pr=87" in result.output
    assert "derived" in result.output


def test_set_pr_skips_display_when_global_already_wired(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _bind_team()
    wired = "#{?#{@hive-pr},PR#{@hive-pr},#I}:#W"
    monkeypatch.setattr("hive.cli.tmux.get_global_window_option", lambda _option: wired)

    result = runner.invoke(cli, ["duo", "set-pr", "87", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["display"] == {
        "window-status-format": "already-global",
        "window-status-current-format": "already-global",
    }
    assert _window_option("hive-pr") == "87"
    # The user wired the display globally — no per-window override installed.
    assert _window_option("window-status-format") is None
    assert _window_option("window-status-current-format") is None


def test_set_pr_reports_skip_for_formats_without_index_token(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _bind_team()
    monkeypatch.setattr("hive.cli.tmux.get_global_window_option", lambda _option: "#W only")

    result = runner.invoke(cli, ["duo", "set-pr", "87", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["display"] == {
        "window-status-format": "skipped-no-index-token",
        "window-status-current-format": "skipped-no-index-token",
    }
    assert _window_option("hive-pr") == "87"
    assert _window_option("window-status-format") is None
    assert _window_option("window-status-current-format") is None


def test_set_pr_rerun_overwrites_stamp_and_rederives_from_global(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()

    assert runner.invoke(cli, ["duo", "set-pr", "87"]).exit_code == 0
    assert runner.invoke(cli, ["duo", "set-pr", "91"]).exit_code == 0

    assert _window_option("hive-pr") == "91"
    # Re-derived from the *global* value, never the window-local one — the
    # second run must not recursively wrap the first run's derived output.
    assert _window_option("window-status-format") == DERIVED_DEFAULT
    assert _window_option("window-status-current-format") == DERIVED_DEFAULT


def test_clear_pr_json_removes_stamp_and_keeps_status_formats(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()
    assert runner.invoke(cli, ["duo", "set-pr", "87"]).exit_code == 0

    result = runner.invoke(cli, ["duo", "clear-pr", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {"window": "dev:0", "previous": "87"}
    assert _window_option("hive-pr") is None
    assert _window_option("window-status-format") == DERIVED_DEFAULT
    assert _window_option("window-status-current-format") == DERIVED_DEFAULT


def test_clear_pr_human_output_reports_previous_stamp(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()
    assert runner.invoke(cli, ["duo", "set-pr", "87"]).exit_code == 0

    result = runner.invoke(cli, ["duo", "clear-pr", "--plain"])

    assert result.exit_code == 0, result.output
    assert "cleared" in result.output
    assert "87" in result.output
    assert _window_option("hive-pr") is None


def test_clear_pr_idempotent_without_stamp(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()

    result = runner.invoke(cli, ["duo", "clear-pr", "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {"window": "dev:0", "previous": None}
    assert _window_option("hive-pr") is None


def test_clear_pr_requires_hive_team_window(runner, configure_hive_home):
    configure_hive_home()

    result = runner.invoke(cli, ["duo", "clear-pr"])

    assert result.exit_code == 1
    assert "@hive-team" in result.output
    assert _window_option("hive-pr") is None


def test_set_pr_fails_outside_tmux(runner, configure_hive_home):
    configure_hive_home(tmux_inside=False)

    result = runner.invoke(cli, ["duo", "set-pr", "87"])

    assert result.exit_code == 1
    # The root-level CLI gate ("Hive requires tmux") fires before the
    # command body's own check — either layer must leave options unwritten.
    assert "tmux" in result.output
    _assert_nothing_written()


def test_set_pr_rejects_nonpositive_number(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()

    result = runner.invoke(cli, ["duo", "set-pr", "0"])

    assert result.exit_code == 1
    assert "positive" in result.output
    assert _window_option("hive-pr") is None
    assert _window_option("window-status-format") is None


def test_set_pr_rejects_non_integer(runner, configure_hive_home):
    configure_hive_home()
    _bind_team()

    result = runner.invoke(cli, ["duo", "set-pr", "abc"])

    assert result.exit_code == 2
    assert _window_option("hive-pr") is None


def test_set_pr_requires_hive_team_window(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    renames = _capture_renames(monkeypatch)

    result = runner.invoke(cli, ["duo", "set-pr", "87"])

    assert result.exit_code == 1
    assert "@hive-team" in result.output
    _assert_nothing_written()
    assert renames == []


def test_no_top_level_pr_jump_command(runner):
    """Scope guard: the jump command was explicitly not selected."""
    result = runner.invoke(cli, ["pr", "87"])
    assert result.exit_code != 0
    assert "no such command" in result.output.lower()
