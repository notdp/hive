import json

from hive import bus
import hive.cli as cli_module
from hive.cli import cli

FIXED_ID = "ab12"


def _patch_ack(monkeypatch):
    """Disable ACK resolution so tests don't need a real transcript."""
    monkeypatch.setattr("hive.cli.secrets.token_hex", lambda _n=2: FIXED_ID)
    monkeypatch.setattr(
        "hive.cli._resolve_ack_baseline",
        lambda _target: (_ for _ in ()).throw(RuntimeError("no transcript")),
        raising=False,
    )


def _assert_frontmatter_envelope(
    envelope: str,
    *,
    message_id: str,
    from_agent: str,
    to_agent: str,
    intent: str,
    body: str,
    artifact: str = "",
) -> None:
    assert envelope.startswith("```HIVE\n")
    assert envelope.endswith("\n```")
    assert f"id: {message_id}" in envelope
    assert f"from: {from_agent}" in envelope
    assert f"to: {to_agent}" in envelope
    if intent:
        assert f"intent: {intent}" in envelope
    if artifact:
        assert f"artifact: {artifact}" in envelope
    else:
        assert "artifact:" not in envelope
    assert "---\n" in envelope
    assert body in envelope


def test_send_injects_hive_envelope_into_target_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    artifact = tmp_path / "review.md"
    artifact.write_text("review request")
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

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(
        cli,
        [
            "send",
            "gpt",
            "please review this",
            "--from",
            "claude",
            "--artifact",
            str(artifact),
        ],
    )

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "intent": "send",
        "from": "claude",
        "to": "gpt",
        "artifact": str(artifact),
        "path": payload["path"],
        "summary": "please review this",
        "ack": "skipped",
    }
    assert payload["path"].endswith(".json")
    assert len(sent) == 1
    envelope = sent[0]
    assert envelope.startswith("```HIVE\n")
    assert envelope.endswith("\n```")
    assert f"id: {FIXED_ID}" in envelope
    assert "from: claude" in envelope
    assert "to: gpt" in envelope
    assert "intent: send" in envelope
    assert f"artifact: {artifact}" in envelope
    assert "please review this" in envelope
    assert len(bus.read_all_events(workspace)) == 1
    assert bus.read_all_events(workspace)[0]["intent"] == "send"


def test_send_supports_structured_intent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
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

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(
        cli,
        [
            "send",
            "gpt",
            "please choose",
            "--from",
            "claude",
            "--intent",
            "ask",
        ],
    )

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "intent": "ask",
        "from": "claude",
        "to": "gpt",
        "artifact": "",
        "path": payload["path"],
        "summary": "please choose",
        "ack": "skipped",
    }
    assert payload["path"].endswith(".json")
    assert len(sent) == 1
    envelope = sent[0]
    assert "```HIVE" in envelope
    assert "intent: ask" in envelope
    assert "please choose" in envelope





def test_send_requires_tmux(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["send", "gpt", "hello from current context"])

    assert result.exit_code != 0
    assert "requires tmux" in result.output


def test_send_requires_live_registered_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

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

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["send", "gpt", "hello"])
    assert result.exit_code != 0
    assert "not alive" in result.output


def test_reply_writes_event_and_projects_status(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    artifact = tmp_path / "review.md"
    artifact.write_text("review result")
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
            assert name == "orch"
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(
        cli,
        [
            "reply",
            "orch",
            "review complete",
            "--from",
            "claude",
            "--artifact",
            str(artifact),
            "--state",
            "done",
            "--meta",
            "verdict=issues",
        ],
    )

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "intent": "reply",
        "from": "claude",
        "to": "orch",
        "artifact": str(artifact),
        "path": payload["path"],
        "summary": "review complete",
        "state": "done",
        "metadata": {"verdict": "issues"},
        "ack": "skipped",
    }
    assert payload["path"].endswith(".json")
    assert len(sent) == 1
    envelope = sent[0]
    assert "```HIVE" in envelope
    assert "intent: reply" in envelope
    assert f"artifact: {artifact}" in envelope
    assert "review complete" in envelope
    assert bus.read_status(workspace, "claude") == {
        "agent": "claude",
        "state": "done",
        "summary": "review complete",
        "artifact": str(artifact),
        "metadata": {"verdict": "issues"},
        "updatedAt": bus.read_all_events(workspace)[0]["createdAt"],
    }


def test_reply_validates_structured_waiting_and_blocked_states(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    class _FakeAgent:
        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            raise AssertionError(f"should not send: {text}")

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    waiting_result = runner.invoke(
        cli,
        ["reply", "orch", "need input", "--state", "waiting_input"],
    )
    assert waiting_result.exit_code != 0
    assert "waiting_input requires --waiting-on or --waiting-for" in waiting_result.output

    blocked_result = runner.invoke(
        cli,
        ["reply", "orch", "blocked", "--state", "blocked"],
    )
    assert blocked_result.exit_code != 0
    assert "blocked requires --blocked-by" in blocked_result.output


def test_inject_delegates_to_agent(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    sent: list[str] = []

    class _FakeAgent:
        def send(self, text: str) -> None:
            sent.append(text)

    class _FakeTeam:
        tmux_session = "dev"
        tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["inject", "claude", "plain prompt"])
    assert result.exit_code == 0
    assert sent == ["plain prompt"]
    assert "Injected raw input into claude." in result.output


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


def test_kill_removes_agent(runner, configure_hive_home, monkeypatch):
    configure_hive_home()
    killed: list[str] = []

    class _FakeAgent:
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
    assert "Killed opus." in result.output
    assert "opus" not in _FakeTeam.agents


def test_notify_uses_current_pane_by_default(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%72")
    monkeypatch.setattr(
        "hive.cli.notify_ui.notify",
        lambda message, pane_id, seconds, highlight, window_status: {
            "message": message,
            "paneId": pane_id,
            "seconds": seconds,
            "highlight": highlight,
            "windowStatus": window_status,
        },
    )

    result = runner.invoke(cli, ["notify", "按 Tab 和我对话"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload == {
        "message": "按 Tab 和我对话",
        "paneId": "%72",
        "seconds": 12,
        "highlight": False,
        "windowStatus": True,
    }


def test_notify_fails_outside_tmux(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "")

    result = runner.invoke(cli, ["notify", "需要确认"])

    assert result.exit_code == 1
    assert "requires tmux" in result.output


def test_internal_notify_hook_command_delegates_to_notify_hook_main(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.notify_hook.main", lambda: 0)

    result = runner.invoke(cli, ["_notify-hook"])

    assert result.exit_code == 0


# --- ACK-specific tests ---


def test_send_ack_confirmed(runner, configure_hive_home, monkeypatch, tmp_path):
    """ACK returns confirmed when nonce appears in transcript."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    sent: list[str] = []

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            sent.append(text)
            # Simulate CLI accepting input — write a user turn with the id.
            transcript.write_text(
                '{"type": "user", "message": {"role": "user", "content": "'
                + f"id: {FIXED_ID}"
                + '"}}\n'
            )

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli.secrets.token_hex", lambda _n=2: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)

    result = runner.invoke(cli, ["send", "gpt", "test", "--from", "claude"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["ack"] == "confirmed"


def test_send_ack_unconfirmed_on_timeout(runner, configure_hive_home, monkeypatch, tmp_path):
    """ACK returns unconfirmed when transcript never shows the nonce."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

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

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli.secrets.token_hex", lambda _n=2: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.adapters.base.wait_for_id_in_transcript", lambda path, message_id, baseline, timeout=45.0: False)

    result = runner.invoke(cli, ["send", "gpt", "test", "--from", "claude"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["ack"] == "unconfirmed"


def test_send_ack_skipped_when_transcript_unresolvable(runner, configure_hive_home, monkeypatch, tmp_path):
    """ACK gracefully degrades to skipped when transcript cannot be found."""
    configure_hive_home()
    _patch_ack(monkeypatch)
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

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

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))

    result = runner.invoke(cli, ["send", "gpt", "test", "--from", "claude"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["ack"] == "skipped"


def test_reply_ack_confirmed(runner, configure_hive_home, monkeypatch, tmp_path):
    """Reply also gets ACK — confirmed when nonce found in transcript."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    transcript.write_text("")

    class _FakeAgent:
        pane_id = "%99"

        def is_alive(self) -> bool:
            return True

        def send(self, text: str) -> None:
            transcript.write_text(
                '{"type": "message", "message": {"role": "user", "content": "'
                + f"id: {FIXED_ID}"
                + '"}}\n'
            )

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli.secrets.token_hex", lambda _n=2: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)

    result = runner.invoke(cli, ["reply", "orch", "done", "--from", "worker", "--state", "done"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["ack"] == "confirmed"
