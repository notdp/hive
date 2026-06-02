"""Tests for `hive gang cleanup`.

Cleanup scans current gang's `<main>-peer-<N>` teams, kills their tmux
windows, and clears leftover `@hive-*` window options. Invariants:
  - no flags / positional args (timing is orch-skill-enforced, not CLI)
  - must run from a gang pane; running from a peer pane is rejected
  - main gang window (orch / skeptic) is never touched
  - JSON output shape: `{killedWindows: [...], killedTeams: [...]}`
"""

import json

import pytest

from hive.cli import _is_peer_team_name, cli


# --- _is_peer_team_name helper ---


@pytest.mark.parametrize(
    "name,expected",
    [
        ("613-6", False),
        ("613-6-peer-1", True),
        ("613-6-peer-1000", True),
        ("613-6-peer-1002", True),
        ("613-6-peer-", False),       # empty suffix
        ("613-6-peer-abc", False),    # non-numeric suffix
        ("peer-1000", False),         # no `-peer-` infix, only prefix
        ("team-peer-xyz-1000", False),  # `-peer-` exists but trailing suffix isn't digits
        ("613-6-peer-1-peer-2", True),  # nested `-peer-<N>`; rightmost wins
        ("", False),
    ],
)
def test_is_peer_team_name(name, expected):
    assert _is_peer_team_name(name) is expected


# --- CLI surface ---


def test_cleanup_help_has_no_flags(runner):
    """No --force / --dry-run / --yes etc. — timing is orch-skill-enforced."""
    result = runner.invoke(cli, ["gang", "cleanup", "--help"])
    assert result.exit_code == 0, result.output
    out = result.output.lower()
    for banned in ("--force", "--dry-run", "--yes", "--all", "--peer"):
        assert banned not in out, f"cleanup leaked a flag: {banned}"


def test_cleanup_rejects_positional_args(runner):
    """No positional arguments accepted."""
    result = runner.invoke(cli, ["gang", "cleanup", "peer-1000"])
    assert result.exit_code != 0
    assert "unexpected" in result.output.lower() or "got" in result.output.lower()


# --- behavior ---


def _stub_kill_window(monkeypatch):
    killed: list[str] = []
    monkeypatch.setattr("hive.cli.tmux.kill_window", lambda target: killed.append(target))
    return killed


def _prep_gang_with_peers(configure_hive_home, monkeypatch, peer_indices):
    """Set up a fake gang with main team `dev-0` + peers at the given indices.

    Returns the list of expected peer window targets so the test can assert.
    """
    configure_hive_home(current_pane="%100", session_name="dev")
    import hive.cli as cli_mod

    # Tag current pane as orch of main gang team (using a real gang instance
    # name from the canonical pool; legacy literal "gang" is rejected).
    cli_mod.tmux.tag_pane("%100", "agent", "peaky.orch", "dev-0", group="peaky")
    # Main team window is dev:0 (configured by the fixture's get_current_window_target).
    cli_mod.tmux.set_window_option("dev:0", "@hive-team", "dev-0")
    cli_mod.tmux.set_window_option("dev:0", "@hive-workspace", "/tmp/ws")

    expected_windows: list[str] = []
    for i, n in enumerate(peer_indices, start=1):
        peer_team = f"dev-0-peer-{n}"
        peer_win = f"dev:{i}"
        cli_mod.tmux.set_window_option(peer_win, "@hive-team", peer_team)
        cli_mod.tmux.set_window_option(peer_win, "@hive-workspace", "/tmp/ws")
        cli_mod.tmux.set_window_option(peer_win, "@hive-created", "0")
        expected_windows.append(peer_win)

    return expected_windows


def test_cleanup_from_main_kills_all_peers(runner, configure_hive_home, monkeypatch):
    """Main gang pane + 2 peer teams → cleanup kills 2 windows, emits JSON,
    and leaves the main window's @hive-team intact.
    """
    expected = _prep_gang_with_peers(configure_hive_home, monkeypatch, [1000, 1001])
    killed = _stub_kill_window(monkeypatch)

    import hive.cli as cli_mod

    result = runner.invoke(cli, ["gang", "cleanup"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert sorted(payload["killedWindows"]) == sorted(expected)
    assert sorted(payload["killedTeams"]) == ["dev-0-peer-1000", "dev-0-peer-1001"]

    # kill_window was invoked for each peer window.
    assert sorted(killed) == sorted(expected)

    # Peer window @hive-team options were cleared.
    for win in expected:
        assert cli_mod.tmux.get_window_option(win, "hive-team") is None

    # Main window tag survives.
    assert cli_mod.tmux.get_window_option("dev:0", "hive-team") == "dev-0"


def test_cleanup_with_no_peers_succeeds_with_empty_arrays(
    runner, configure_hive_home, monkeypatch
):
    """No peer-* teams → cleanup is a no-op but still exits 0."""
    _prep_gang_with_peers(configure_hive_home, monkeypatch, [])
    killed = _stub_kill_window(monkeypatch)

    result = runner.invoke(cli, ["gang", "cleanup"])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    assert payload == {"killedWindows": [], "killedTeams": []}
    assert killed == []


def test_cleanup_rejected_from_peer_pane(runner, configure_hive_home, monkeypatch):
    """Running cleanup from a peer pane (team name ending in -peer-<N>) is
    rejected — it's the orch's job, and doing it from inside a peer window
    would race the cleanup's own kill.
    """
    configure_hive_home(current_pane="%512", session_name="dev")
    import hive.cli as cli_mod

    # Simulate peer pane: group=<gang> but team name is <main>-peer-<N>.
    cli_mod.tmux.tag_pane("%512", "agent", "peaky.worker-1000", "dev-0-peer-1000", group="peaky")
    # Current pane's window is dev:0 per fixture; bind it to the peer team so
    # _resolve_scoped_team picks the peer name up.
    cli_mod.tmux.set_window_option("dev:0", "@hive-team", "dev-0-peer-1000")
    cli_mod.tmux.set_window_option("dev:0", "@hive-workspace", "/tmp/ws")

    killed = _stub_kill_window(monkeypatch)

    result = runner.invoke(cli, ["gang", "cleanup"])
    assert result.exit_code != 0
    # Fail message calls out peer-team context.
    assert "peer" in result.output.lower()
    # Critically: no tmux window got killed on rejection.
    assert killed == []


def test_cleanup_rejected_from_non_gang_pane(runner, configure_hive_home, monkeypatch):
    """Pane without @hive-group=gang is rejected — e.g. a plain daily agent pane."""
    configure_hive_home(current_pane="%900", session_name="dev")
    import hive.cli as cli_mod

    # Tag as a normal agent with no group.
    cli_mod.tmux.tag_pane("%900", "agent", "solo", "daily-team")
    cli_mod.tmux.set_window_option("dev:0", "@hive-team", "daily-team")

    killed = _stub_kill_window(monkeypatch)

    result = runner.invoke(cli, ["gang", "cleanup"])
    assert result.exit_code != 0
    assert "gang" in result.output.lower()
    assert killed == []
