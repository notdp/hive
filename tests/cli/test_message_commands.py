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


def _patch_ack(monkeypatch):
    """Disable ACK resolution so tests don't need a real transcript."""
    monkeypatch.setattr(
        "hive.sidecar._resolve_ack_baseline",
        lambda _target: (_ for _ in ()).throw(RuntimeError("no transcript")),
        raising=False,
    )


def _patch_sidecar_requests(monkeypatch, team_obj, *, pending=None):
    if pending is None:
        pending = {}

    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)

    def _resolve_live_agent(_team_name: str, agent_name: str):
        agent = team_obj.get(agent_name)
        if not agent.is_alive():
            raise RuntimeError(f"agent '{agent_name}' is not alive")
        return team_obj, agent

    monkeypatch.setattr("hive.sidecar._resolve_live_agent", _resolve_live_agent)
    monkeypatch.setattr(
        "hive.sidecar._agent_runtime_payload",
        lambda _pane_id: {
            "alive": True,
            "turnPhase": "turn_closed",
        },
    )

    def _request_team_runtime(_workspace: str, *, team: str):
        from hive.sidecar import _agent_runtime_payload

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
        from hive.sidecar import _send_payload

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

    monkeypatch.setattr("hive.sidecar.request_send", _request_send)
    monkeypatch.setattr("hive.sidecar.request_team_runtime", _request_team_runtime)
    return pending




def test_send_injects_hive_envelope_into_target_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
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
    _patch_sidecar_requests(monkeypatch, team)

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
    payload = json.loads(result.output)
    assert "from" not in payload
    assert payload["to"] == "gpt"
    assert payload["artifact"] == artifact
    assert "summary" not in payload
    assert "delivery" not in payload
    assert "injectStatus" not in payload
    assert "turnObserved" not in payload
    assert "followUp" not in payload
    assert len(sent) == 1
    assert payload["msgId"] == FIXED_ID
    assert sent == [f"<HIVE from=claude to=gpt msgId={FIXED_ID} artifact={artifact}>\nplease review this\n</HIVE>"]
    events = bus.read_all_events(workspace)
    assert [e["intent"] for e in events] == ["send"]



def test_send_does_not_defer_root_send_when_turn_phase_is_unknown(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
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
    _patch_sidecar_requests(monkeypatch, team)
    monkeypatch.setattr(
        "hive.sidecar._agent_runtime_payload",
        lambda _pane_id: {
            "alive": True,
            "turnPhase": "assistant_text_idle",
        },
    )

    result = runner.invoke(cli, ["send", "gpt", "please review this", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["msgId"]
    assert len(sent) == 1
    assert sent[0].startswith("<HIVE from=claude to=gpt ")



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


def test_reply_auto_fills_reply_to_from_latest_inbound(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    inbound = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="see patch")

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "ack, looking"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "from" not in payload
    assert payload["to"] == "dodo"
    assert payload["autoReplyTo"] == inbound.msg_id
    events = bus.read_all_events(workspace)
    outbound = [event for event in events if event.get("from") == "orch" and event.get("to") == "dodo"]
    assert len(outbound) == 1
    assert outbound[0].get("inReplyTo") == inbound.msg_id


def test_reply_fails_when_no_inbound_from_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "late answer"])

    assert result.exit_code != 0
    assert "no recent message from 'dodo'" in result.output
    assert sent == []


def test_reply_fails_when_latest_inbound_already_replied(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
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
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "one more thing"])

    assert result.exit_code != 0
    assert "already replied to" in result.output
    assert "pass --reply-to explicitly" in result.output


def test_reply_honors_explicit_reply_to_override(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    first = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="older msg")
    second = bus.write_send_event(workspace, from_agent="dodo", to_agent="orch", body="newer msg")
    bus.write_send_event(
        workspace,
        from_agent="orch",
        to_agent="dodo",
        body="auto",
        reply_to=second.msg_id,
    )

    sent: list[str] = []
    team = _reply_fake_team(workspace, sent_transcript=sent)
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["reply", "dodo", "on older thread", "--reply-to", first.msg_id])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert "autoReplyTo" not in payload
    events = bus.read_all_events(workspace)
    latest_outbound = [
        event for event in events if event.get("from") == "orch" and event.get("to") == "dodo"
    ][-1]
    assert latest_outbound.get("inReplyTo") == first.msg_id


def test_send_rejects_legacy_to_option_with_positional_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    called = []
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: called.append("resolved") or ("team-x", object()),
    )

    result = runner.invoke(cli, ["send", "--to", "gpt", "--msg", "hello"])

    assert result.exit_code == 2  # UsageError: argument-shape failures match Click parser errors
    assert "Usage: " in result.output
    assert "hive send takes positional args" in result.output
    assert "Drop --to/--msg" in result.output
    assert called == []  # Guard must short-circuit before touching the team.


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
    assert "for help" in result.output  # Click's Try-help hint line
    assert "hive send requires <agent>" in result.output
    assert "Drop --to/--msg" not in result.output
    assert called == []


def test_reply_rejects_legacy_msg_option_with_positional_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    called = []
    monkeypatch.setattr(
        "hive.cli._resolve_scoped_team",
        lambda _team, required=True: called.append("resolved") or ("team-x", object()),
    )

    result = runner.invoke(cli, ["reply", "dodo", "--msg", "hello"])

    assert result.exit_code == 2
    assert "Usage: " in result.output
    assert "hive reply takes positional args" in result.output
    assert "Drop --to/--msg" in result.output
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
    _patch_sidecar_requests(monkeypatch, team)

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
    _patch_ack(monkeypatch)
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
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "test", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert "delivery" not in payload
    assert "followUp" not in payload


def test_send_inject_failure_no_sidecar(runner, configure_hive_home, monkeypatch, tmp_path):
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
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    _patch_sidecar_requests(monkeypatch, team)

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


def _gate_test_setup(monkeypatch, tmp_path, transcript_records=None):
    """Common setup for gate tests. Returns (workspace, transcript, sent list)."""
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    if transcript_records is not None:
        transcript.write_text(
            "\n".join(json.dumps(r) for r in transcript_records) + "\n"
        )
    else:
        transcript.write_text("")

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
    monkeypatch.setattr("hive.sidecar._resolve_ack_baseline", lambda _target: (transcript, transcript.stat().st_size), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")
    # Gate tests only care about the gate projection; collapse the 3s grace loop.
    _patch_sidecar_requests(monkeypatch, team)

    return workspace, transcript, sent


def test_send_blocked_by_gate(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    artifact = _write_artifact(tmp_path, "gate-blocked.md")
    _gate_test_setup(monkeypatch, tmp_path, transcript_records=[
        {"type": "user", "message": {"role": "user", "content": "do something"}},
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "proceed?"}},
                ],
            },
        },
    ])

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code != 0
    assert "waiting for a user answer" in result.output


def test_gate_fail_open_no_transcript(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
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
    _patch_sidecar_requests(monkeypatch, team)

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert "delivery" not in payload
    # gate field was removed — send still succeeds fail-open without a transcript.
    assert "gate" not in payload
    assert "injectStatus" not in payload


def test_gate_clear_is_omitted_from_send_output(runner, configure_hive_home, monkeypatch, tmp_path):
    """When transcript resolves and gate is clear, the gate field is omitted (default is noise)."""
    configure_hive_home()
    artifact = _write_artifact(tmp_path, "gate-clear.md")
    workspace, transcript, sent = _gate_test_setup(monkeypatch, tmp_path, transcript_records=[
        {"type": "user", "message": {"role": "user", "content": "hello"}},
    ])

    result = runner.invoke(cli, ["send", "gpt", "hello", "--artifact", artifact])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    # gate=clear is the default noise-free case and is omitted from output.
    assert "gate" not in payload


def _patch_send_failed(monkeypatch, workspace):
    """Make _request_send_payload return a delivery=failed payload without touching the sidecar."""

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


def test_reply_exits_nonzero_when_transport_refuses(runner, configure_hive_home, monkeypatch, tmp_path):
    """`hive reply` must mirror `send` and exit non-zero on transport refusal."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)
    _patch_send_failed(monkeypatch, workspace)
    # reply needs an anchor msgId; pass one explicitly so auto-resolution isn't required
    result = runner.invoke(cli, ["reply", "gpt", "ack", "--reply-to", FIXED_ID])

    assert result.exit_code == 1, f"expected exit 1 on transport refusal, got {result.exit_code}: {result.output}"
    assert "transport refused" in result.output


def test_send_exits_zero_on_accepted(runner, configure_hive_home, monkeypatch, tmp_path):
    """An accepted send exits 0 with just the message identity."""
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
    payload = json.loads(result.output)
    assert "delivery" not in payload
