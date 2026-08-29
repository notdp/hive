import json
from types import SimpleNamespace

import pytest

from hive import bus
import hive.cli as cli_module
from hive.cli import cli

FIXED_ID = bus.format_msg_id(1)


def _write_artifact(tmp_path, name: str = "details.md", content: str = "details") -> str:
    path = tmp_path / name
    path.write_text(content)
    return str(path)


def _patch_hived_requests(monkeypatch, team_obj, *, pending=None, runtime=None):
    if pending is None:
        pending = {}
    if runtime is None:
        runtime = {"alive": True, "turnPhase": "turn_closed"}

    monkeypatch.setattr("hive.hived.ensure_hived", lambda *a, **kw: 4321)

    def _resolve_live_agent(_team_name: str, agent_name: str):
        agent = team_obj.get(agent_name)
        if not agent.is_alive():
            raise RuntimeError(f"agent '{agent_name}' is not alive")
        return team_obj, agent

    monkeypatch.setattr("hive.hived._resolve_live_agent", _resolve_live_agent)
    monkeypatch.setattr(
        "hive.hived._agent_runtime_payload",
        lambda _pane_id, **_kw: dict(runtime),
    )

    def _request_team_runtime(_workspace: str, *, team: str):
        from hive.hived import _agent_runtime_payload

        members_payload = {}
        member_map = getattr(team_obj, "members", None)
        if not isinstance(member_map, dict):
            member_map = getattr(team_obj, "agents", None)
        if not isinstance(member_map, dict):
            member_map = {}
        for name, agent in member_map.items():
            payload = _agent_runtime_payload(getattr(agent, "pane_id", "") or "")
            payload["alive"] = bool(agent.is_alive())
            members_payload[name] = payload
        return {"ok": True, "team": team, "members": members_payload}

    def _request_send(
        workspace: str,
        *,
        team: str,
        sender_agent: str,
        sender_pane: str,
        target_agent: str,
        body: str,
        artifact: str = "",
        reply_to: str = "",
    ):
        from hive.hived import _send_payload

        try:
            return _send_payload(
                workspace=workspace,
                team_name=team,
                sender_agent=sender_agent,
                sender_pane=sender_pane,
                target_agent=target_agent,
                body=body,
                artifact=artifact,
                reply_to=reply_to,
            )
        except Exception as exc:
            return {"ok": False, "error": str(exc)}

    monkeypatch.setattr("hive.hived.request_send", _request_send)
    monkeypatch.setattr("hive.hived.request_team_runtime", _request_team_runtime)
    return pending




def test_send_injects_hive_envelope_into_target_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    artifact = _write_artifact(tmp_path, "review.md", "review request")
    bus.init_workspace(workspace)

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            assert name == "gpt"
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_hived_requests(monkeypatch, team)

    result = runner.invoke(
        cli,
        [
            "send",
            "gpt",
            "please review this",
            "--artifact",
            artifact,
        ],
    )

    assert result.exit_code == 0
    assert result.output == ""  # fire-and-forget: success is silent
    assert len(sent) == 1
    assert sent == [f"<HIVE from=claude to=gpt msgId={FIXED_ID} artifact={artifact}>\nplease review this\n</HIVE>"]
    events = bus.read_all_events(workspace)
    assert [e["intent"] for e in events] == ["send"]
    assert events[0]["msgId"] == FIXED_ID



def test_send_does_not_defer_root_send_when_turn_phase_is_unknown(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "unknown.md", "full details")

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"
        cli = "claude"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            assert name == "gpt"
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    _patch_hived_requests(
        monkeypatch, team, runtime={"alive": True, "turnPhase": "assistant_text_idle"}
    )

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0
    assert result.output == ""
    assert len(sent) == 1
    assert sent[0].startswith("<HIVE from=claude to=gpt ")


def _claude_member_team(workspace, sent):
    class _FakeAgent:
        pane_id = "%99"
        cli = "claude"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            assert name == "gpt"
            return _FakeAgent()

    return _FakeTeam()


def _wire_claude_send(monkeypatch, team, *, busy):
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr("hive.cli.resolve_session_id_for_pane", lambda _pane_id, profile=None: "sess-1")
    monkeypatch.setattr("hive.hived._claude_registry_busy", lambda _pane_id: busy)
    _patch_hived_requests(monkeypatch, team)


def test_send_to_a_busy_claude_member_delivers_now(runner, configure_hive_home, monkeypatch, tmp_path):
    """No hived hold: `priority: next` rides the receiver's own queue."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    sent: list[str] = []
    _wire_claude_send(monkeypatch, _claude_member_team(workspace, sent), busy=True)

    result = runner.invoke(cli, ["send", "gpt", "please review this"])

    assert result.exit_code == 0, result.output
    assert result.output == ""
    assert len(sent) == 1 and "please review this" in sent[0]


def test_send_to_an_idle_claude_member_delivers_now(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    sent: list[str] = []
    _wire_claude_send(monkeypatch, _claude_member_team(workspace, sent), busy=False)

    result = runner.invoke(cli, ["send", "gpt", "please review this"])

    assert result.exit_code == 0, result.output
    assert result.output == ""
    assert len(sent) == 1


def _reply_fake_team(workspace, *, sent_transcript):
    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent_transcript.append(text)

    class _FakeTeam:
        def __init__(self) -> None:
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    return _FakeTeam()


def test_send_auto_anchors_to_latest_unanswered_inbound(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    inbound = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="see patch")

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_hived_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "dodo", "ack, looking"])

    assert result.exit_code == 0, result.output
    assert result.output == ""
    events = bus.read_all_events(workspace)
    outbound = [event for event in events if event.get("from") == "orch" and event.get("to") == "dodo"]
    assert len(outbound) == 1
    assert outbound[0].get("inReplyTo") == inbound.msg_id


def test_send_opens_root_thread_when_no_inbound(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_hived_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "dodo", "fresh topic"])

    assert result.exit_code == 0, result.output
    assert result.output == ""
    events = bus.read_all_events(workspace)
    assert events[-1].get("inReplyTo") is None or events[-1].get("inReplyTo") == ""


def test_send_opens_root_thread_when_latest_inbound_already_answered(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    inbound = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="see patch")
    bus.write_send_event(
        workspace,
        from_agent="orch",
        to_agent="dodo",
        body="thanks, looking",
        reply_to=inbound.msg_id,
    )

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_hived_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "dodo", "one more thing"])

    assert result.exit_code == 0, result.output
    assert result.output == ""
    latest_outbound = [
        event for event in bus.read_all_events(workspace)
        if event.get("from") == "orch" and event.get("to") == "dodo"
    ][-1]
    assert not latest_outbound.get("inReplyTo")


def test_send_root_protocol_skipped_for_anchored_continuation(runner, configure_hive_home, monkeypatch, tmp_path):
    """A thread continuation may carry a long body; a root send may not."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    long_body = "x" * 600

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_hived_requests(monkeypatch, team)

    root = runner.invoke(cli, ["send", "dodo", long_body])
    assert root.exit_code != 0
    assert "must stay short and unstructured" in root.output

    inbound = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="see patch")
    anchored = runner.invoke(cli, ["send", "dodo", long_body])
    assert anchored.exit_code == 0, anchored.output
    assert anchored.stdout == ""  # silent even when anchored; the bus row carries the link
    latest_outbound = [
        event for event in bus.read_all_events(workspace)
        if event.get("from") == "orch" and event.get("to") == "dodo"
    ][-1]
    assert latest_outbound.get("inReplyTo") == inbound.msg_id


def test_send_without_agent_surfaces_usage_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    called = []
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: called.append("resolved") or ("team-x", object()),
    )

    result = runner.invoke(cli, ["send"])

    assert result.exit_code == 2
    assert "Usage: " in result.output
    assert "Missing argument" in result.output  # Click's own parser error
    assert called == []


def test_send_requires_tmux(runner, configure_hive_home, monkeypatch):
    # Hermetic: the root gate now admits a Claude-session guest (identified by
    # its inbox-socket env); a plain shell outside tmux must still be told to
    # start tmux, so the host session must not leak into this test.
    configure_hive_home(tmux_inside=False)

    result = runner.invoke(cli, ["send", "gpt", "hello from current context"])

    assert result.exit_code != 0
    assert "requires tmux" in result.output


def test_send_requires_live_registered_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "live-agent.md")

    class _DeadAgent:
        def is_alive(self) -> bool:
            return False

        def send(self, text: str) -> None:
            raise AssertionError("should not send")

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _DeadAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    _patch_hived_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])
    assert result.exit_code != 0
    assert "not alive" in result.output


def test_inject_writes_raw_composer_keystrokes(runner, configure_hive_home, monkeypatch):
    # inject is the documented low-level bypass: raw keystrokes for every CLI,
    # never the channel/RPC delivery paths it exists to debug.
    configure_hive_home()
    typed: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        "hive.cli._submit_interactive_text",
        lambda pane, text, cli_name: typed.append((pane, text, cli_name)),
    )

    class _FakeAgent:
        pane_id = "%11"
        cli = "claude"

        def send(self, text: str) -> None:
            raise AssertionError("inject must bypass Agent.send")

    class _FakeTeam:
        name = "team-x"
        tmux_session = "dev"
        tmux_window = "dev:0"
        workspace = ""

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["inject", "claude", "plain prompt"])
    assert result.exit_code == 0
    assert typed == [("%11", "plain prompt", "claude")]
    payload = json.loads(result.output)
    assert payload == {
        "member": "claude",
        "action": "inject",
        "pane": "%11",
        "success": True,
    }


def test_compact_self_delivers_slash_compact_via_composer(runner, configure_hive_home, monkeypatch):
    # no --pane: compact resolves the CURRENT pane from its tmux options, never
    # through _resolve_scoped_team / _resolve_sender / t.get (re-resolving by name
    # is the cross-window same-name bug). A team-bound claude pane delivers
    # /compact through the composer and keeps the team-member output shape.
    configure_hive_home()
    typed: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        "hive.cli._submit_interactive_text",
        lambda pane, text, cli_name: typed.append((pane, text, cli_name)),
    )

    def _no_team(*_a, **_kw):
        raise AssertionError("no-`--pane` compact must use current-pane facts, not team resolution")

    monkeypatch.setattr("hive.cli._resolve_sender", _no_team)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", _no_team)
    monkeypatch.setattr("hive.cli._load_team", _no_team)

    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%21")
    pane_options = {
        ("%21", "hive-team"): "team-x",
        ("%21", "hive-agent"): "orch",
        ("%21", "hive-cli"): "claude",
    }
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )

    result = runner.invoke(cli, ["compact"])
    assert result.exit_code == 0, result.output
    # /compact is a TUI slash command: composer keystrokes, never Agent.send
    # (a channel message would arrive as content, not as a command)
    assert typed == [("%21", "/compact", "claude")]
    payload = json.loads(result.output)
    assert payload == {
        "member": "orch",
        "action": "compact",
        "pane": "%21",
        "status": "compacted",
        "success": True,
    }


def test_compact_with_pane_uses_pane_options(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    built: list[dict] = []
    sent: list[tuple[str, str]] = []

    class _RecordingAgent:
        def __init__(self, **kwargs):
            built.append(kwargs)
            self.pane_id = kwargs.get("pane_id", "")
            self.cli = kwargs.get("cli", "")

        def send(self, text: str) -> None:
            sent.append((self.pane_id, text))

    monkeypatch.setattr("hive.cli.Agent", _RecordingAgent)

    # `--pane` binds the Agent to that literal pane; it must NOT consult
    # _resolve_sender / _resolve_scoped_team (which read the current pane) nor
    # _load_team (re-resolving by name is the cross-window bug).
    def _fail(*_a, **_kw):
        raise AssertionError("--pane must bind to the literal pane, not re-resolve")

    monkeypatch.setattr("hive.cli._resolve_sender", _fail)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", _fail)
    monkeypatch.setattr("hive.cli._load_team", _fail)

    pane_options = {
        ("%42", "hive-team"): "team-x",
        ("%42", "hive-agent"): "bobo",
        ("%42", "hive-cli"): "codex",
    }
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )

    compacted: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.compact_pane",
        lambda pane: compacted.append(pane) or "compacted",
    )

    result = runner.invoke(cli, ["compact", "--pane", "%42"])
    assert result.exit_code == 0, result.output
    # codex compaction goes over the daemon RPC, not the composer/keystrokes, so
    # no Agent is built — the literal-pane guarantee is proven by the RPC target.
    assert sent == []
    assert built == []
    assert compacted == ["%42"]  # RPC fired at the literal pane
    payload = json.loads(result.output)
    assert payload == {
        "member": "bobo",
        "action": "compact",
        "pane": "%42",
        "status": "compacted",
        "success": True,
    }


def test_compact_with_pane_targets_literal_pane_not_same_named_agent(
    runner, configure_hive_home, monkeypatch
):
    """Regression: two duos can end up sharing a derived team name, so one team
    holds two agents named `validator` in different windows. `compact --pane
    <here>` must compact <here>, never the same-named agent's pane elsewhere —
    which is exactly what re-resolving via `_load_team` + `t.get(name)` did."""
    configure_hive_home()
    sent: list[str] = []

    class _RecordingAgent:
        def __init__(self, **kwargs):
            self.pane_id = kwargs.get("pane_id", "")
            self.cli = kwargs.get("cli", "")

        def send(self, text: str) -> None:
            sent.append(self.pane_id)

    monkeypatch.setattr("hive.cli.Agent", _RecordingAgent)

    def _no_team(*_a, **_kw):
        raise AssertionError("--pane must not re-resolve through the team")

    monkeypatch.setattr("hive.cli._load_team", _no_team)

    # %40 is window 3's validator, but its @hive-team collides with window 2's
    # team (0-2), whose registered `validator` lives at a different pane.
    pane_options = {
        ("%40", "hive-team"): "0-2",
        ("%40", "hive-agent"): "validator",
        ("%40", "hive-cli"): "codex",
    }
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )

    compacted: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.compact_pane",
        lambda pane: compacted.append(pane) or "compacted",
    )

    result = runner.invoke(cli, ["compact", "--pane", "%40"])
    assert result.exit_code == 0, result.output
    # codex -> RPC compaction; the literal pane is what gets compacted.
    assert sent == []
    assert compacted == ["%40"]  # window 3's own pane, not the other window's validator


def test_compact_rejects_non_agent_pane(runner, configure_hive_home, monkeypatch):
    # An untagged pane is no longer rejected for lacking a team — non-team panes
    # are supported. It is rejected only when it is not an agent pane at all: no
    # hive-cli tag AND no detectable agent profile.
    configure_hive_home()
    monkeypatch.setattr("hive.cli.tmux.get_pane_option", lambda _pane, _key: None)
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: None)

    result = runner.invoke(cli, ["compact", "--pane", "%99"])
    assert result.exit_code != 0
    assert "unsupported agent pane" in result.output


def test_compact_codex_busy_keystrokes_disabled_notice(runner, configure_hive_home, monkeypatch):
    # codex mid-turn: compact_pane returns "busy"; compact_cmd keystrokes
    # /compact into the TUI so codex shows its own "disabled while a task is in
    # progress" refusal — it must NOT fire the RPC at a busy agent and must NOT
    # fall back to turn/start send().
    configure_hive_home()

    class _RecordingAgent:
        def __init__(self, **kwargs):
            self.pane_id = kwargs.get("pane_id", "")
            self.cli = kwargs.get("cli", "")

        def send(self, text: str) -> None:
            raise AssertionError("codex compact must not use turn/start send()")

    monkeypatch.setattr("hive.cli.Agent", _RecordingAgent)
    pane_options = {
        ("%42", "hive-team"): "team-x",
        ("%42", "hive-agent"): "bobo",
        ("%42", "hive-cli"): "codex",
    }
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )
    monkeypatch.setattr("hive.adapters.codex_app_server.compact_pane", lambda _pane: "busy")
    keyed: list[tuple] = []
    monkeypatch.setattr(
        "hive.cli._submit_interactive_text",
        lambda pane, text, cli: keyed.append((pane, text, cli)),
    )

    result = runner.invoke(cli, ["compact", "--pane", "%42"])
    assert result.exit_code == 0, result.output
    assert keyed == [("%42", "/compact", "codex")]  # codex itself surfaces the refusal
    payload = json.loads(result.output)
    assert payload == {
        "member": "bobo",
        "action": "compact",
        "pane": "%42",
        "status": "busy",
        "success": False,
    }


def test_compact_grok_idle_fires_leader_rpc(runner, configure_hive_home, monkeypatch):
    # grok is daemon-backed too: an idle pane compacts over the leader RPC
    # (x.ai/compact_conversation), never through the composer.
    configure_hive_home()

    class _RecordingAgent:
        def __init__(self, **kwargs):
            pass

        def send(self, _text: str) -> None:
            raise AssertionError("grok compact must not use session/prompt send()")

    monkeypatch.setattr("hive.cli.Agent", _RecordingAgent)
    pane_options = {
        ("%42", "hive-team"): "team-x",
        ("%42", "hive-agent"): "bobo",
        ("%42", "hive-cli"): "grok",
    }
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )
    monkeypatch.setattr(
        "hive.cli._submit_interactive_text",
        lambda *_a: (_ for _ in ()).throw(AssertionError("idle grok must not be keystroked")),
    )
    compacted: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.grok_leader.compact_pane",
        lambda pane: compacted.append(pane) or "compacted",
    )

    result = runner.invoke(cli, ["compact", "--pane", "%42"])
    assert result.exit_code == 0, result.output
    assert compacted == ["%42"]
    payload = json.loads(result.output)
    assert payload == {
        "member": "bobo",
        "action": "compact",
        "pane": "%42",
        "status": "compacted",
        "success": True,
    }


@pytest.mark.parametrize("status", ["busy", "unavailable"])
def test_compact_grok_not_compacted_keystrokes_the_tui(
    runner, configure_hive_home, monkeypatch, status
):
    # A busy (or leader-less) grok gets `/compact` keystroked into its own TUI so
    # grok surfaces the refusal itself, exactly like the codex path.
    configure_hive_home()
    pane_options = {
        ("%42", "hive-team"): "team-x",
        ("%42", "hive-agent"): "bobo",
        ("%42", "hive-cli"): "grok",
    }
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )
    monkeypatch.setattr("hive.adapters.grok_leader.compact_pane", lambda _pane: status)
    keyed: list[tuple] = []
    monkeypatch.setattr(
        "hive.cli._submit_interactive_text",
        lambda pane, text, cli_name: keyed.append((pane, text, cli_name)),
    )

    result = runner.invoke(cli, ["compact", "--pane", "%42"])
    assert result.exit_code == 0, result.output
    assert keyed == [("%42", "/compact", "grok")]
    payload = json.loads(result.output)
    assert payload["status"] == status
    assert payload["success"] is False


def _forbid_team_resolution(monkeypatch):
    """Make any Team load/resolve a hard failure (non-team compact must avoid it)."""
    def _no_team(*_a, **_kw):
        raise AssertionError("non-team compact must not resolve or load a Team")

    monkeypatch.setattr("hive.cli._resolve_scoped_team", _no_team)
    monkeypatch.setattr("hive.cli._resolve_sender", _no_team)
    monkeypatch.setattr("hive.cli._load_team", _no_team)


def test_compact_pane_non_team_codex_fires_rpc(runner, configure_hive_home, monkeypatch):
    # A pane with no hive-team is still compactable. codex compacts over the
    # daemon RPC at the literal pane; member is the pane id and the payload
    # carries team: null. No Agent is built and no Team is touched.
    configure_hive_home()
    _forbid_team_resolution(monkeypatch)
    built: list[dict] = []

    class _RecordingAgent:
        def __init__(self, **kwargs):
            built.append(kwargs)

        def send(self, _text: str) -> None:
            raise AssertionError("codex compact must not send through the composer")

    monkeypatch.setattr("hive.cli.Agent", _RecordingAgent)

    pane_options = {("%42", "hive-cli"): "codex"}  # no hive-team / hive-agent
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )
    compacted: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.compact_pane",
        lambda pane: compacted.append(pane) or "compacted",
    )

    result = runner.invoke(cli, ["compact", "--pane", "%42"])
    assert result.exit_code == 0, result.output
    assert built == [] and compacted == ["%42"]
    payload = json.loads(result.output)
    assert payload == {
        "member": "%42",
        "action": "compact",
        "pane": "%42",
        "status": "compacted",
        "success": True,
        "team": None,
    }


def test_compact_pane_non_team_non_codex_sends_composer(runner, configure_hive_home, monkeypatch):
    # Non-team, non-codex pane: /compact is delivered through the composer on the
    # literal pane. The Agent is bound to the pane with an empty team name and the
    # pane id as its name.
    configure_hive_home()
    _forbid_team_resolution(monkeypatch)
    typed: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        "hive.cli._submit_interactive_text",
        lambda pane, text, cli_name: typed.append((pane, text, cli_name)),
    )

    pane_options = {("%42", "hive-cli"): "claude"}  # no hive-team / hive-agent
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )

    result = runner.invoke(cli, ["compact", "--pane", "%42"])
    assert result.exit_code == 0, result.output
    assert typed == [("%42", "/compact", "claude")]
    payload = json.loads(result.output)
    assert payload == {
        "member": "%42",
        "action": "compact",
        "pane": "%42",
        "status": "compacted",
        "success": True,
        "team": None,
    }


def test_compact_self_non_team_codex_uses_current_pane(runner, configure_hive_home, monkeypatch):
    # no --pane on a non-team codex pane: resolve the CURRENT pane and compact it
    # over the RPC. member is the pane id; payload carries team: null.
    configure_hive_home()
    _forbid_team_resolution(monkeypatch)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%7")

    pane_options = {("%7", "hive-cli"): "codex"}
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )
    compacted: list[str] = []
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.compact_pane",
        lambda pane: compacted.append(pane) or "compacted",
    )

    result = runner.invoke(cli, ["compact"])
    assert result.exit_code == 0, result.output
    assert compacted == ["%7"]
    payload = json.loads(result.output)
    assert payload == {
        "member": "%7",
        "action": "compact",
        "pane": "%7",
        "status": "compacted",
        "success": True,
        "team": None,
    }


def test_compact_self_non_team_non_codex_uses_current_pane(runner, configure_hive_home, monkeypatch):
    # no --pane on a non-team claude pane: resolve the CURRENT pane and deliver
    # /compact through the composer.
    configure_hive_home()
    _forbid_team_resolution(monkeypatch)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%7")
    typed: list[tuple[str, str, str]] = []
    monkeypatch.setattr(
        "hive.cli._submit_interactive_text",
        lambda pane, text, cli_name: typed.append((pane, text, cli_name)),
    )

    pane_options = {("%7", "hive-cli"): "claude"}
    monkeypatch.setattr(
        "hive.cli.tmux.get_pane_option",
        lambda pane, key: pane_options.get((pane, key)),
    )

    result = runner.invoke(cli, ["compact"])
    assert result.exit_code == 0, result.output
    assert typed == [("%7", "/compact", "claude")]
    payload = json.loads(result.output)
    assert payload == {
        "member": "%7",
        "action": "compact",
        "pane": "%7",
        "status": "compacted",
        "success": True,
        "team": None,
    }


def test_capture_reads_agent_output(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    class _FakeAgent:
        def capture(self, lines: int) -> str:
            assert lines == 12
            return "captured output"

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["capture", "claude", "--lines", "12"])
    assert result.exit_code == 0
    assert result.output.strip() == "captured output"


def test_interrupt_delegates_to_agent(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    calls: list[str] = []

    class _FakeAgent:
        pane_id = "%12"

        def interrupt(self) -> None:
            calls.append("interrupt")

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["interrupt", "claude"])
    assert result.exit_code == 0
    assert calls == ["interrupt"]
    payload = json.loads(result.output)
    assert payload == {
        "member": "claude",
        "action": "interrupt",
        "pane": "%12",
        "success": True,
    }


@pytest.mark.parametrize(
    "argv", [["inject", "ghost", "hi"], ["interrupt", "ghost"]], ids=["inject", "interrupt"]
)
def test_unknown_member_fails_with_a_message_not_a_traceback(runner, configure_hive_home, monkeypatch, argv):
    configure_hive_home()

    class _FakeTeam:
        name = "team-x"
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, name: str):
            raise KeyError(f"Agent '{name}' not found")

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, argv)
    assert result.exit_code == 1
    assert isinstance(result.exception, SystemExit)  # no KeyError traceback
    assert "member 'ghost' not found in team 'team-x'" in result.output


def test_kill_removes_agent(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    killed: list[str] = []

    class _FakeAgent:
        pane_id = "%13"

        def kill(self) -> None:
            killed.append("killed")

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"
        agents = {"opus": _FakeAgent()}

        def get(self, name: str):
            return self.agents[name]

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["kill", "opus"])
    assert result.exit_code == 0
    assert killed == ["killed"]
    payload = json.loads(result.output)
    assert payload == {
        "member": "opus",
        "action": "kill",
        "pane": "%13",
        "removedFromTeam": True,
        "success": True,
    }
    assert "opus" not in _FakeTeam.agents


def test_notify_uses_current_pane_by_default(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%72")
    monkeypatch.setattr(
        "hive.cli.notify_ui.notify",
        lambda message, pane_id: {
            "message": message,
            "paneId": pane_id,
            "surface": "fired",
        },
    )

    result = runner.invoke(cli, ["notify", "按 Tab 和我对话"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "message": "按 Tab 和我对话",
        "paneId": "%72",
        "surface": "fired",
    }


def test_notify_fails_outside_tmux(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "")

    result = runner.invoke(cli, ["notify", "需要确认"])

    assert result.exit_code == 1
    assert "requires tmux" in result.output


# --- ACK-specific tests ---


def test_parse_control_mode_output_decodes_octal_escape():
    """Decode \\NNN sequences so all-digit msgIds don't false-match escape boundaries."""
    from hive.tmux import parse_control_mode_output

    # Raw line has \012 (LF) followed by literal '3'; undecoded substring would contain '0123'.
    pane_id, payload = parse_control_mode_output("%output %99 before\\0123after")
    assert pane_id == "%99"
    assert "0123" not in payload
    assert payload == "before\n3after"


def test_parse_control_mode_output_strips_extended_prefix():
    """%extended-output carries 'age ... : payload'; strip up to first colon."""
    from hive.tmux import parse_control_mode_output

    pane_id, payload = parse_control_mode_output("%extended-output %99 1234 : msgId=abc1 body")
    assert pane_id == "%99"
    assert payload == "msgId=abc1 body"


def test_send_ack_skipped_when_transcript_unresolvable(runner, configure_hive_home, monkeypatch, tmp_path):
    """ACK gracefully degrades to skipped when transcript cannot be found."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "ack-skipped.md")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            pass

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_hived_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact])

    assert result.exit_code == 0
    assert result.output == ""


def test_send_inject_failure_no_hived(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "inject-failure.md")

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    class _BrokenAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            raise RuntimeError("boom")

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _BrokenAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_hived_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact])

    # transport refusal surfaces as a standard operational failure
    assert result.exit_code == 1
    assert "transport refused" in result.output


def test_send_help_explains_delivery_states(runner):
    result = runner.invoke(cli, ["send", "--help"])
    help_text = " ".join(result.output.split())

    assert result.exit_code == 0
    assert "Delivery is binary" in help_text
    assert "queued" not in help_text
    assert "pending" not in help_text


def _gate_test_setup(monkeypatch, tmp_path, runtime=None):
    """Common setup for gate tests. Returns (workspace, sent list).

    The send gate reads the member's runtime payload (native daemon /
    registry state), so gate tests parametrize that payload directly.
    """
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_hived_requests(monkeypatch, team, runtime=runtime)

    return workspace, sent


def test_send_blocked_by_gate(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    artifact = _write_artifact(tmp_path, "gate-blocked.md")
    _gate_test_setup(monkeypatch, tmp_path, runtime={
        "alive": True,
        "inputState": "waiting_user",
        "inputReason": "registry:input needed",
    })

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code != 0
    assert "waiting for a user answer" in result.output


def test_gate_waives_dialog_open_waiting(runner, configure_hive_home, monkeypatch, tmp_path):
    # a /status-style dialog in an attached viewer parks status on waiting,
    # but the inbox still queues normally — that reason never blocks a send
    configure_hive_home()
    artifact = _write_artifact(tmp_path, "gate-dialog.md")
    _gate_test_setup(monkeypatch, tmp_path, runtime={
        "alive": True,
        "inputState": "waiting_user",
        "inputReason": "registry:dialog open",
    })

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code == 0


def test_gate_unknown_runtime_state_does_not_block(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    artifact = _write_artifact(tmp_path, "gate-open.md")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            pass

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_hived_requests(monkeypatch, team, runtime={
        "alive": True,
        "inputState": "unknown",
        "inputReason": "no_session",
    })

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code == 0
    # only a proven waiting_user blocks — unknown does not veto the send.
    assert result.output == ""


def test_gate_clear_is_omitted_from_send_output(runner, configure_hive_home, monkeypatch, tmp_path):
    """When the member is ready, the gate field is omitted (default is noise)."""
    configure_hive_home()
    artifact = _write_artifact(tmp_path, "gate-clear.md")
    workspace, sent = _gate_test_setup(monkeypatch, tmp_path, runtime={
        "alive": True,
        "inputState": "ready",
        "inputReason": "",
    })

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code == 0
    # a ready member is the noise-free default: silent success.
    assert result.output == ""


def _patch_send_failed(monkeypatch, workspace):
    """Make _request_send_payload return a delivery=failed payload without touching the hived."""

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_send_target_team", lambda _agent: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr(
        "hive.cli._request_send_payload",
        lambda **_kw: (_ for _ in ()).throw(RuntimeError("transport refused gpt: no channel")),
    )


def test_answer_command_is_removed(runner):
    """The answer command was removed; the CLI must not know it at all."""
    result = runner.invoke(cli, ["answer", "gpt", "yes"])
    assert result.exit_code != 0
    assert "No such command" in result.output


def test_send_exits_nonzero_when_transport_refuses(runner, configure_hive_home, monkeypatch, tmp_path):
    """`hive send` must exit non-zero when delivery=failed so shell `&&` chains respect failure."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _patch_send_failed(monkeypatch, workspace)

    result = runner.invoke(cli, ["send", "gpt", "please review"])

    assert result.exit_code == 1, f"expected exit 1 on transport refusal, got {result.exit_code}: {result.output}"
    assert "transport refused" in result.output


def test_send_exits_zero_on_accepted(runner, configure_hive_home, monkeypatch, tmp_path):
    """An accepted send exits 0 and prints nothing."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

    team = _FakeTeam()
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_send_target_team", lambda _agent: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    monkeypatch.setattr(
        "hive.cli._request_send_payload",
        lambda **_kw: {
            "to": "gpt",
            "msgId": FIXED_ID,
        },
    )

    result = runner.invoke(cli, ["send", "gpt", "hi"])
    assert result.exit_code == 0
    assert result.output == ""


def test_send_to_flow_run_walks_the_bare_name_lane_and_confirms(runner, configure_hive_home, monkeypatch):
    # The canonical mailbox address must not fall into qualified-agent
    # resolution ("agent 'flow.run' not found"), and a mailbox delivery is
    # the one send that prints: there is no peer runtime to be silent about.
    configure_hive_home()
    team_obj = type("T", (), {"name": "t2"})()
    monkeypatch.setattr("hive.cli._default_team", lambda: "t2")
    monkeypatch.setattr("hive.cli._load_team", lambda name, prefer_pane="": team_obj)
    monkeypatch.setattr("hive.cli._find_qualified_agent_target", lambda a: pytest.fail("mailbox must not resolve as an agent"))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _p: "impl")
    monkeypatch.setattr("hive.cli._resolve_workspace", lambda t, required: "/tmp/ws-t2")
    monkeypatch.setattr("hive.bus.latest_inbound_send_event", lambda *a, **kw: None)
    sent = {}
    monkeypatch.setattr(
        "hive.cli._request_send_payload",
        lambda **kw: sent.update(kw) or {"ok": True, "msgId": "m9", "mailbox": True},
    )

    result = runner.invoke(cli, ["send", "flow.run", "done: see artifact"])

    assert result.exit_code == 0, result.output
    assert sent["target_agent"] == "flow.run"
    assert "delivered to flow mailbox msgId=m9" in result.output


def test_flow_is_not_a_team_name(runner, configure_hive_home):
    from hive.team import validate_team_name

    assert "flow" in validate_team_name("flow")
