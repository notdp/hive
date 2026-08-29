"""hive attach: materialize a team's display from the registry."""
import json

from hive.cli import cli


def _entry(monkeypatch, members=None):
    from hive import registry

    assert registry.record_team(
        team="honey", workspace="/tmp/ws-h", created_at="7.0",
        members=members if members is not None else [
            {"name": "orch", "cli": "claude", "sessionId": "job-1", "cwd": "/repo"},
            {"name": "rex", "cli": "grok", "sessionId": "sid-g", "cwd": "/repo"},
            {"name": "val", "cli": "codex", "sessionId": "tid-9", "cwd": "/repo"},
        ],
    ) == "written"


def _display_mocks(monkeypatch):
    calls = {"windows": [], "splits": [], "keys": [], "tags": [], "opts": [], "selected": [], "sessions": []}
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: "dev")
    monkeypatch.setattr("hive.cli.tmux.has_session", lambda name: name == "dev")
    monkeypatch.setattr(
        "hive.cli.tmux.new_session",
        lambda name, **kw: calls["sessions"].append(name) or "%49",
    )
    monkeypatch.setattr(
        "hive.cli.tmux.new_window",
        lambda session, name, cwd, detach: calls["windows"].append((session, name, cwd)) or (f"{session}:9", "%50"),
    )
    panes = iter(["%51", "%52", "%53"])
    monkeypatch.setattr(
        "hive.cli.tmux.split_window",
        lambda target, horizontal, cwd=None, detach=True: calls["splits"].append(target) or next(panes),
    )
    monkeypatch.setattr("hive.cli.tmux.set_pane_title", lambda pane, title: None)
    monkeypatch.setattr(
        "hive.cli.tmux.tag_pane",
        lambda pane, role, agent, team, cli="", group="": calls["tags"].append((pane, agent, cli)),
    )
    monkeypatch.setattr(
        "hive.cli.tmux.send_keys", lambda pane, cmd: calls["keys"].append((pane, cmd))
    )
    monkeypatch.setattr("hive.cli.tmux.configure_hive_window", lambda w: None)
    monkeypatch.setattr(
        "hive.cli.tmux.set_window_option",
        lambda w, k, v: calls["opts"].append((k, v)),
    )
    monkeypatch.setattr("hive.cli.tmux.get_window_id", lambda w: "@9")
    monkeypatch.setattr("hive.layout.apply_adaptive", lambda w: None)
    monkeypatch.setattr("hive.cli.tmux.select_window", lambda w: calls["selected"].append(w))
    monkeypatch.setattr("hive.cli._ensure_team_hived", lambda t, ws: None)
    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda sid, **kw: {"id": sid})
    return calls


def test_attach_builds_window_with_member_attach_panes(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _entry(monkeypatch)
    monkeypatch.setattr("hive.team._find_team_window", lambda name, prefer_pane="": ("", {}))
    calls = _display_mocks(monkeypatch)

    result = runner.invoke(cli, ["attach", "honey"])

    assert result.exit_code == 0, result.output
    assert calls["windows"] == [("dev", "honey", "/repo")]
    # orch first, then members; each pane tagged BEFORE its launcher runs
    tagged = {a: (p, c) for p, a, c in calls["tags"]}
    assert set(tagged) == {"orch", "rex", "val"}
    cmds = {pane: cmd for pane, cmd in calls["keys"]}
    orch_pane = tagged["orch"][0]
    assert "hive claude --resume job-1" in cmds[orch_pane]
    rex_pane = tagged["rex"][0]
    assert "hive grok --resume sid-g" in cmds[rex_pane]
    val_pane = tagged["val"][0]
    assert "hive codex resume tid-9" in cmds[val_pane]
    tag_order = [k for k, _ in calls["keys"]]
    for pane, _a, _c in calls["tags"]:
        assert pane in tag_order
    # display cache updated
    from hive import registry

    assert registry.load("honey")["display"] == "@9"
    # window carries the display options for the hived/border layer
    assert ("@hive-team", "honey") in calls["opts"]
    assert calls["selected"] == ["dev:9"]


def test_attach_jumps_to_an_existing_window(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _entry(monkeypatch)
    monkeypatch.setattr(
        "hive.team._find_team_window", lambda name, prefer_pane="": ("dev:3", {"window_id": "@3"})
    )
    selected = []
    monkeypatch.setattr("hive.cli.tmux.select_window", lambda w: selected.append(w))
    monkeypatch.setattr("hive.cli._ensure_team_hived", lambda t, ws: None)

    result = runner.invoke(cli, ["attach", "honey"])

    assert result.exit_code == 0, result.output
    assert selected == ["dev:3"]
    assert "found dev:3" in result.output


def test_attach_unknown_team_fails(runner, configure_hive_home):
    configure_hive_home()
    result = runner.invoke(cli, ["attach", "nosuch"])
    assert result.exit_code != 0
    assert "not found" in result.output


def test_attach_skips_members_without_engine_identity(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _entry(monkeypatch, members=[
        {"name": "orch", "cli": "claude", "sessionId": "job-1", "cwd": "/repo"},
        {"name": "ghost", "cli": "grok", "sessionId": "", "cwd": "/repo"},
    ])
    monkeypatch.setattr("hive.team._find_team_window", lambda name, prefer_pane="": ("", {}))
    calls = _display_mocks(monkeypatch)

    result = runner.invoke(cli, ["attach", "honey"])

    assert result.exit_code == 0, result.output
    assert "ghost" in result.output  # warned on stderr
    assert {a for _p, a, _c in calls["tags"]} == {"orch"}


def test_attach_outside_tmux_creates_fallback_session(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _entry(monkeypatch)
    monkeypatch.setattr("hive.team._find_team_window", lambda name, prefer_pane="": ("", {}))
    calls = _display_mocks(monkeypatch)
    monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: None)
    monkeypatch.setattr("hive.cli.tmux.has_session", lambda name: False)
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    attached = []
    monkeypatch.setattr(
        "hive.cli.tmux.exec_attach", lambda session, window: attached.append((session, window))
    )

    result = runner.invoke(cli, ["attach", "honey"])

    assert result.exit_code == 0, result.output
    assert calls["sessions"] == ["hive"]
    assert calls["windows"] == [("hive", "honey", "/repo")]
    assert attached == [("hive", "hive:9")]


def test_attach_renders_an_interactive_session_as_a_readonly_viewer(runner, configure_hive_home, monkeypatch):
    # A desktop/joined session must never be resumed into a fork: its pane
    # gets the read-only transcript viewer instead.
    configure_hive_home()
    _entry(monkeypatch, members=[
        {"name": "orch", "cli": "claude", "sessionId": "ccd-sid-7", "cwd": "/repo"},
    ])
    monkeypatch.setattr("hive.team._find_team_window", lambda name, prefer_pane="": ("", {}))
    calls = _display_mocks(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_bg.job_row", lambda sid, **kw: None)

    result = runner.invoke(cli, ["attach", "honey"])

    assert result.exit_code == 0, result.output
    cmds = {pane: cmd for pane, cmd in calls["keys"]}
    orch_cmd = next(iter(cmds.values()))
    assert "hive view ccd-sid-7" in orch_cmd
    assert "--resume" not in orch_cmd
