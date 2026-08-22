"""CLI tests for `hive ccd ls` / `hive ccd send` (reach a Claude session outside the team)."""
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


def test_ccd_send_delivers_to_the_named_session_as_the_calling_member(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch, team="duo-1", agent="validator")
    sent: list[tuple] = []
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()] if name == "desk" else [])
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: sent.append((sock, text, sender)) or "udsWriteAccepted",
    )
    result = runner.invoke(cli, ["ccd", "send", "desk", "build is green, merge when ready"])
    assert result.exit_code == 0, result.output
    # the frame's `from` never reaches the receiving model, so the body rides
    # inside the ordinary <HIVE> envelope and self-identifies in band
    assert sent == [(
        "/tmp/cc-socks/4242.sock",
        "<HIVE from=hive:duo-1.validator to=ccd:desk>\nbuild is green, merge when ready\n</HIVE>",
        "hive:duo-1.validator",
    )]
    assert json.loads(result.output) == {
        "session": "desk", "title": "", "pid": 4242, "cwd": "/w/desk",
        "from": "hive:duo-1.validator", "accepted": "udsWriteAccepted",
    }


def test_ccd_send_signs_as_the_calling_session_when_not_a_member(runner, configure_hive_home, monkeypatch):
    # a Claude session outside any team signs as itself, so the receiver can
    # answer with `hive ccd send "<name>"` instead of guessing who wrote
    configure_hive_home(tmux_inside=False)
    _identity(monkeypatch, team=None, agent=None)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.self_session",
        lambda: _session(name="my-own-session", pid=9),
    )
    sent: list[tuple] = []
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: sent.append(sender) or "udsWriteAccepted",
    )
    result = runner.invoke(cli, ["ccd", "send", "desk", "hi"])
    assert result.exit_code == 0, result.output
    assert sent == ["ccd:my-own-session"]


def test_ccd_send_wraps_the_body_in_a_hive_envelope(runner, configure_hive_home, monkeypatch):
    # no msgId (not a bus thread): just <HIVE from=… to=ccd:<target>>body</HIVE>
    configure_hive_home(tmux_inside=False)
    _identity(monkeypatch, team=None, agent=None)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.self_session",
        lambda: _session(name="my-own-session", pid=9),
    )
    texts: list[str] = []
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: texts.append(text) or "udsWriteAccepted",
    )
    runner.invoke(cli, ["ccd", "send", "desk", "hello there"])
    assert texts == ["<HIVE from=ccd:my-own-session to=ccd:desk>\nhello there\n</HIVE>"]


def test_ccd_send_outside_any_team_sends_as_plain_hive(runner, configure_hive_home, monkeypatch):
    # a plain shell (no team, not a Claude session) keeps the bare marker
    configure_hive_home(tmux_inside=False)
    _identity(monkeypatch, team=None, agent=None)
    monkeypatch.setattr("hive.adapters.claude_sessions.self_session", lambda: None)
    sent: list[tuple] = []
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.send",
        lambda sock, text, *, sender: sent.append(sender) or "udsWriteAccepted",
    )
    result = runner.invoke(cli, ["ccd", "send", "desk", "hi"])
    assert result.exit_code == 0, result.output
    assert sent == ["hive"]


def test_ccd_send_unknown_session_fails_and_points_at_sessions(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [])
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["ccd", "send", "ghost", "hi"])
    assert result.exit_code != 0
    assert "ghost" in result.output and "hive ccd ls" in result.output


def test_ccd_send_ambiguous_name_fails_without_sending(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.resolve",
        lambda name: [_session(pid=1, cwd="/w/a"), _session(pid=2, cwd="/w/b")],
    )
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: pytest.fail("must not send"))
    result = runner.invoke(cli, ["ccd", "send", "desk", "hi"])
    assert result.exit_code != 0
    assert "2 live sessions" in result.output and "/w/a" in result.output and "/w/b" in result.output
    assert "name or pid" in result.output


def test_ccd_send_unreachable_socket_fails_closed(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: None)
    result = runner.invoke(cli, ["ccd", "send", "desk", "hi"])
    assert result.exit_code != 0
    assert "not listening" in result.output


def test_ccd_send_accepts_the_desktop_title(runner, configure_hive_home, monkeypatch):
    # the human names sessions by what the sidebar shows; resolve() matches
    # title or name, and the output echoes both
    configure_hive_home()
    _identity(monkeypatch, team="t", agent="w")
    asked: list[str] = []
    target = _session(name="nice-almeida-dd", title="PR70 审查")
    monkeypatch.setattr(
        "hive.adapters.claude_sessions.resolve",
        lambda label: asked.append(label) or ([target] if label in ("PR70 审查", "nice-almeida-dd") else []),
    )
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda sock, text, *, sender: "udsWriteAccepted")
    result = runner.invoke(cli, ["ccd", "send", "PR70 审查", "merge when green"])
    assert result.exit_code == 0, result.output
    assert asked == ["PR70 审查"]
    out = json.loads(result.output)
    assert (out["session"], out["title"]) == ("nice-almeida-dd", "PR70 审查")


def test_ccd_send_stalled_receiver_is_reported_as_stalled_not_gone(runner, configure_hive_home, monkeypatch):
    # a listener that accepted but never read the frame is a different failure
    # from an absent listener; the message must not claim the session exited
    configure_hive_home()
    _identity(monkeypatch)
    monkeypatch.setattr("hive.adapters.claude_sessions.resolve", lambda name: [_session()])
    monkeypatch.setattr("hive.adapters.claude_sessions.send", lambda *a, **k: "udsWriteTimedOut")
    result = runner.invoke(cli, ["ccd", "send", "desk", "x" * 20000])
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
    # <HIVE from=...> attribute tokenization on the receiving side
    assert sent["sender_agent"] == "ccd:nice-dd"
    assert sent["target_agent"] == "validator"
    assert sent["team"] is team_obj
    assert json.loads(result.output)["msgId"] == "m1"


def test_guest_send_with_ambiguous_member_asks_for_team(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _guest(monkeypatch)
    monkeypatch.setattr(
        "hive.cli.tmux.list_panes_all", lambda: [_pane("worker", "t1"), _pane("worker", "t2")]
    )
    monkeypatch.setattr("hive.cli._request_send_payload", lambda **kw: pytest.fail("must not send"))
    result = runner.invoke(cli, ["send", "worker", "hi"])
    assert result.exit_code != 0
    assert "t1" in result.output and "t2" in result.output and "--team" in result.output


def test_guest_send_honours_explicit_team(runner, configure_hive_home, monkeypatch):
    configure_hive_home(tmux_inside=False)
    _guest(monkeypatch, title="", name="plain-name")
    team_obj = type("T", (), {"name": "t2"})()
    monkeypatch.setattr("hive.cli._load_team", lambda name, prefer_pane="": team_obj)
    monkeypatch.setattr("hive.cli._existing_team_agent", lambda t, a: object())
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required=True: "/ws")
    sent: dict = {}
    monkeypatch.setattr(
        "hive.cli._request_send_payload",
        lambda **kw: sent.update(kw) or {"to": kw["target_agent"], "msgId": "m2"},
    )
    result = runner.invoke(cli, ["send", "worker", "hi", "--team", "t2"])
    assert result.exit_code == 0, result.output
    assert sent["sender_agent"] == "ccd:plain-name"  # no title: the name stands in


def test_member_send_rejects_the_team_flag(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    result = runner.invoke(cli, ["send", "worker", "hi", "--team", "t2"])
    assert result.exit_code != 0
    assert "outside tmux" in result.output


@pytest.mark.parametrize("command", ["send", "reply"])
def test_ccd_labelled_target_is_redirected_to_ccd_send(runner, configure_hive_home, monkeypatch, command):
    # a member answering a guest must not look for a team member named ccd:…
    configure_hive_home()
    result = runner.invoke(cli, [command, "ccd:PR70 审查", "hi"])
    assert result.exit_code != 0
    assert 'hive ccd send "PR70 审查"' in result.output
