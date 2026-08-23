"""CLI tests for messaging Claude sessions outside the team.

One command, one address space: `hive send ccd.<session>` pushes into an
outside session's inbox, `hive send <team>.<member>` reaches in from one,
and every envelope's `from=` value is the verbatim reply address.
`hive ccd ls` is discovery only.
"""
import json

import pytest

from hive.adapters.claude_sessions import ClaudeSession
from hive.cli import cli

pytestmark = pytest.mark.cli


def _session(name="desk", pid=4242, cwd="/w/desk", sock="/tmp/cc-socks/4242.sock", title=""):
    return ClaudeSession(name=name, pid=pid, cwd=cwd, kind="interactive", socket_path=sock, title=title)


def _identity(monkeypatch, team="t", agent="w"):
    monkeypatch.setattr("hive.cli._default_team", lambda: team)
    monkeypatch.setattr("hive.cli._default_agent", lambda: agent)


def test_ccd_ls_lists_reachable_sessions(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)  # tmux-optional: a human terminal can ask
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.list_sessions",
        lambda: [_session(), _session(name="other", pid=7, cwd="/w/o", sock="/tmp/cc-socks/7.sock")],
    )
    result = runner.invoke(cli, ["ccd", "ls"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.output) == {"sessions": [
        {"name": "desk", "title": "", "pid": 4242, "kind": "interactive", "cwd": "/w/desk"},
        {"name": "other", "title": "", "pid": 7, "kind": "interactive", "cwd": "/w/o"},
    ]}


def test_ccd_ls_marks_sessions_that_are_team_members(runner, configure_hive_home, monkeypatch):
    # a hive member IS a Claude session; the listing must say so, or a reader
    # would inbox-push a teammate and bypass the bus
    configure_hive_home()
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.list_sessions",
        lambda: [_session(), _session(name="ordo-c1", pid=99, cwd="/w/ordo", sock="/tmp/cc-socks/99.sock")],
    )
    monkeypatch.setattr("hive.cli._live_member_pids", lambda: {99: ("t1", "worker")})
    result = runner.invoke(cli, ["ccd", "ls"])
    assert result.exit_code == 0, result.output
    rows = json.loads(result.output)["sessions"]
    assert "member" not in rows[0]
    assert rows[1]["member"] == "t1.worker"


# ---- outbound: a member pushes into an outside session (`hive send ccd.<x>`) ----


def test_send_ccd_delivers_to_the_named_session_as_the_calling_member(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch, team="duo-1", agent="validator")
    sent: list[tuple] = []
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()] if name == "desk" else [])
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: sent.append((sock, text, sender)) or "udsWriteAccepted",
    )
    result = runner.invoke(cli, ["send", "ccd.desk", "build is green, merge when ready"])
    assert result.exit_code == 0, result.output
    # the frame's `from` never reaches the receiving model, so the body rides
    # inside the ordinary <HIVE> envelope and self-identifies in band; the
    # from value IS the reply address (`hive send duo-1.validator`)
    assert sent == [(
        "/tmp/cc-socks/4242.sock",
        "<HIVE from=duo-1.validator to=ccd.desk>\nbuild is green, merge when ready\n</HIVE>",
        "duo-1.validator",
    )]
    assert json.loads(result.output) == {
        "session": "desk", "title": "", "pid": 4242, "cwd": "/w/desk",
        "from": "duo-1.validator", "accepted": "udsWriteAccepted",
    }


@pytest.mark.parametrize(
    ("is_claude_session", "expected"),
    [(True, "SendMessage"), (False, "requires tmux")],  # plain shell dies at the root gate
)
def test_send_ccd_refuses_a_non_member_caller(runner, configure_hive_home, monkeypatch, is_claude_session, expected):
    # member-only: sessions message each other with the native SendMessage tool
    configure_hive_home(tmux_inside=False)
    _identity(monkeypatch, team=None, agent=None)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.self_session",
        lambda: _session(name="my-own-session", pid=9) if is_claude_session else None,
    )
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "ccd.desk", "hi"])
    assert result.exit_code != 0
    assert expected in result.output


def test_send_ccd_refuses_a_teammate_target(runner, configure_hive_home, monkeypatch):
    # the target session is really a member of the caller's own team: the
    # bus owns that conversation (threading, attribution), not the inbox
    configure_hive_home()
    _identity(monkeypatch, team="t", agent="w")
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr("hive.cli._live_member_pids", lambda: {4242: ("t", "validator")})
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "ccd.desk", "hi"])
    assert result.exit_code != 0
    assert "hive send validator" in result.output


def test_send_ccd_refuses_a_member_of_another_team(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch, team="t", agent="w")
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr("hive.cli._live_member_pids", lambda: {4242: ("t9", "worker")})
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "ccd.desk", "hi"])
    assert result.exit_code != 0
    assert "t9.worker" in result.output


def test_send_ccd_refuses_an_artifact(runner, configure_hive_home, monkeypatch):
    # a session push is not a bus thread; there is no artifact channel
    configure_hive_home()
    _identity(monkeypatch, team="t", agent="w")
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "ccd.desk", "hi", "--artifact", "/tmp/x.md"])
    assert result.exit_code != 0
    assert "artifact" in result.output


def test_send_ccd_requires_a_body(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch, team="t", agent="w")
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "ccd.desk"])
    assert result.exit_code != 0


def test_send_ccd_wraps_the_body_in_a_hive_envelope(runner, configure_hive_home, monkeypatch):
    # no msgId (not a bus thread): just <HIVE from=<team>.<agent> to=ccd.<target>>body</HIVE>
    configure_hive_home()
    _identity(monkeypatch, team="t", agent="w")
    texts: list[str] = []
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: texts.append(text) or "udsWriteAccepted",
    )
    runner.invoke(cli, ["send", "ccd.desk", "hello there"])
    assert texts == ["<HIVE from=t.w to=ccd.desk>\nhello there\n</HIVE>"]


def test_send_ccd_unknown_session_fails_and_points_at_sessions(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [])
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "ccd.ghost", "hi"])
    assert result.exit_code != 0
    assert "ghost" in result.output and "hive ccd ls" in result.output


def test_send_ccd_ambiguous_name_fails_without_sending(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.resolve",
        lambda name: [_session(pid=1, cwd="/w/a"), _session(pid=2, cwd="/w/b")],
    )
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "ccd.desk", "hi"])
    assert result.exit_code != 0
    assert "2 live sessions" in result.output and "/w/a" in result.output and "/w/b" in result.output
    assert "name or pid" in result.output


def test_send_ccd_unreachable_socket_fails_closed(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: None)
    result = runner.invoke(cli, ["send", "ccd.desk", "hi"])
    assert result.exit_code != 0
    assert "not listening" in result.output


def test_send_ccd_accepts_the_desktop_title_with_dots_and_spaces(runner, configure_hive_home, monkeypatch):
    # the human names sessions by what the sidebar shows; the address splits
    # on the FIRST dot, so a title containing dots survives; resolve()
    # matches title or name, and the output echoes both
    configure_hive_home()
    _identity(monkeypatch, team="t", agent="w")
    asked: list[str] = []
    target = _session(name="nice-almeida-dd", title="PR 0.70 审查")
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.resolve",
        lambda label: asked.append(label) or ([target] if label in ("PR 0.70 审查", "nice-almeida-dd") else []),
    )
    sent: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: sent.append(text) or "udsWriteAccepted",
    )
    result = runner.invoke(cli, ["send", "ccd.PR 0.70 审查", "merge when green"])
    assert result.exit_code == 0, result.output
    assert asked == ["PR 0.70 审查"]
    out = json.loads(result.output)
    assert (out["session"], out["title"]) == ("nice-almeida-dd", "PR 0.70 审查")
    # the envelope address is the session NAME, never the title: spaces would
    # break <HIVE> attribute tokenization
    assert sent == ["<HIVE from=t.w to=ccd.nice-almeida-dd>\nmerge when green\n</HIVE>"]


def test_send_ccd_stalled_receiver_is_reported_as_stalled_not_gone(runner, configure_hive_home, monkeypatch):
    # a listener that accepted but never read the frame is a different failure
    # from an absent listener; the message must not claim the session exited
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: "udsWriteTimedOut")
    result = runner.invoke(cli, ["send", "ccd.desk", "x" * 20000])
    assert result.exit_code != 0
    assert "did not read" in result.output and "19 KB" in result.output
    assert "exited" not in result.output


# ---- the reverse direction: a Claude session sends into hive as a guest ----


def _guest(monkeypatch, title="PR70 审查", name="nice-dd"):
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.self_session",
        lambda: ClaudeSession(
            name=name, pid=1, cwd="/w", kind="interactive",
            socket_path="/tmp/s.sock", title=title,
        ),
    )


def _pane(agent, team):
    from hive.tmux import PaneInfo

    return PaneInfo("%5", agent, command="codex", role="agent", agent=agent, team=team, cli="codex")


def test_guest_send_reaches_a_member_attributed_to_the_session(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _guest(monkeypatch)
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: [_pane("validator", "t1")])
    team_obj = object()
    monkeypatch.setattr("hive.cli._load_team", lambda name, prefer_pane="": team_obj)
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required=True: "/ws")
    sent: dict = {}

    def _capture(**kw):
        sent.update(kw)
        return {"to": kw["target_agent"], "msgId": "m1"}

    monkeypatch.setattr("hive.cli._request_send_payload", lambda **kw: _capture(**kw))
    result = runner.invoke(cli, ["send", "validator", "check PR 73"])
    assert result.exit_code == 0, result.output
    # the session NAME, not the title: titles may contain spaces, which break
    # <HIVE from=...> attribute tokenization on the receiving side; the value
    # is the verbatim reply address (`hive send ccd.nice-dd`)
    assert sent["sender_agent"] == "ccd.nice-dd"
    assert sent["target_agent"] == "validator"
    assert sent["team"] is team_obj
    assert json.loads(result.output)["msgId"] == "m1"


def test_guest_send_with_ambiguous_member_lists_dotted_addresses(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _guest(monkeypatch)
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_all", lambda: [_pane("worker", "t1"), _pane("worker", "t2")]
    )
    monkeypatch.setattr("hive.cli._request_send_payload", lambda **kw: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "worker", "hi"])
    assert result.exit_code != 0
    assert "t1.worker" in result.output and "t2.worker" in result.output


def _team_exists(monkeypatch, *names):
    monkeypatch.setattr(
        "hive.team._find_team_window",
        lambda name, prefer_pane="": ("dev:9", {}) if name in names else ("", {}),
    )


def test_guest_send_honours_the_dotted_address(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _guest(monkeypatch, title="", name="plain-name")
    _team_exists(monkeypatch, "t2")
    team_obj = type("T", (), {"name": "t2"})()
    loaded: list[str] = []
    monkeypatch.setattr("hive.cli._load_team", lambda name, prefer_pane="": loaded.append(name) or team_obj)
    monkeypatch.setattr("hive.cli._existing_team_agent", lambda t, a: object())
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required=True: "/ws")
    sent: dict = {}
    monkeypatch.setattr(
        "hive.cli._request_send_payload",
        lambda **kw: sent.update(kw) or {"to": kw["target_agent"], "msgId": "m2"},
    )
    result = runner.invoke(cli, ["send", "t2.worker", "hi"])
    assert result.exit_code == 0, result.output
    assert loaded == ["t2"]
    assert sent["target_agent"] == "worker"
    assert sent["sender_agent"] == "ccd.plain-name"  # no title: the name stands in


def test_guest_send_dotted_address_with_unknown_member_fails(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _guest(monkeypatch)
    _team_exists(monkeypatch, "t2")
    team_obj = type("T", (), {"name": "t2"})()
    monkeypatch.setattr("hive.cli._load_team", lambda name, prefer_pane="": team_obj)
    monkeypatch.setattr("hive.cli._existing_team_agent", lambda t, a: None)
    monkeypatch.setattr("hive.cli._request_send_payload", lambda **kw: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "t2.ghost", "hi"])
    assert result.exit_code != 0
    assert "ghost" in result.output and "t2" in result.output


def test_member_send_rejects_the_team_qualified_address(runner, configure_hive_home, monkeypatch):
    # inside a team, peers are bare names; the `<team>.<member>` form is the
    # outside sessions' address and must not grow into cross-team messaging
    configure_hive_home()
    _team_exists(monkeypatch, "t2")
    result = runner.invoke(cli, ["send", "t2.worker", "hi"])
    assert result.exit_code != 0
    assert "bare name" in result.output


def test_member_send_keeps_a_squad_qualified_name_whole(runner, configure_hive_home, monkeypatch):
    # squad members' own names contain a dot (`peaky.orch`); the address
    # splits only when the prefix names a live team, so qualified squad
    # routing must survive untouched
    configure_hive_home()
    _team_exists(monkeypatch)  # "peaky" is a squad prefix, not a team
    asked: list[str] = []
    team_obj = type("T", (), {"name": "honey"})()
    monkeypatch.setattr(
        "hive.cli._resolve_send_target_team",
        lambda to_agent: asked.append(to_agent) or ("honey", team_obj),
    )
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required=True: "/ws")
    sent: dict = {}
    monkeypatch.setattr(
        "hive.cli._request_send_payload",
        lambda **kw: sent.update(kw) or {"to": kw["target_agent"], "msgId": "m3"},
    )
    result = runner.invoke(cli, ["send", "peaky.challenger", "hi"])
    assert result.exit_code == 0, result.output
    assert asked == ["peaky.challenger"]
    assert sent["target_agent"] == "peaky.challenger"


def test_guest_send_keeps_a_squad_qualified_name_whole(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _guest(monkeypatch)
    _team_exists(monkeypatch)  # no team named "peaky"
    monkeypatch.setattr("hive.cli.tmux.list_panes_all", lambda: [_pane("peaky.orch", "honey")])
    team_obj = object()
    monkeypatch.setattr("hive.cli._load_team", lambda name, prefer_pane="": team_obj)
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required=True: "/ws")
    sent: dict = {}
    monkeypatch.setattr(
        "hive.cli._request_send_payload",
        lambda **kw: sent.update(kw) or {"to": kw["target_agent"], "msgId": "m4"},
    )
    result = runner.invoke(cli, ["send", "peaky.orch", "hi"])
    assert result.exit_code == 0, result.output
    assert sent["target_agent"] == "peaky.orch"


def test_live_member_pids_maps_pid_to_team_dot_agent(runner, configure_hive_home, monkeypatch):
    # the REAL helper (not a monkeypatched stand-in): the ccd ls `member`
    # field must be `<team>.<agent>`, the verbatim send address
    configure_hive_home()
    from hive.tmux import PaneInfo

    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_all",
        lambda: [PaneInfo("%5", "", role="agent", agent="validator", team="honey", cli="claude")],
    )
    monkeypatch.setattr("hive.agent_cli.claude_pid_for_pane", lambda pane_id: 4242)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.list_sessions",
        lambda: [_session(name="ordo-c1", pid=4242, cwd="/w/ordo", sock="/tmp/cc-socks/4242.sock")],
    )
    result = runner.invoke(cli, ["ccd", "ls"])
    assert result.exit_code == 0, result.output
    rows = json.loads(result.output)["sessions"]
    assert rows[0]["member"] == "honey.validator"


def test_reply_to_a_session_is_redirected_to_send(runner, configure_hive_home, monkeypatch):
    # a session is not a team member: no thread to anchor, reply must not bus it
    configure_hive_home()
    result = runner.invoke(cli, ["reply", "ccd.PR70 审查", "hi"])
    assert result.exit_code != 0
    assert 'hive send "ccd.PR70 审查"' in result.output
