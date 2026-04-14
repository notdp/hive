import json

from hive import bus
import hive.cli as cli_module
from hive.cli import cli

FIXED_ID = "ab12"


def _patch_ack(monkeypatch):
    """Disable ACK resolution so tests don't need a real transcript."""
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr(
        "hive.cli._resolve_ack_baseline",
        lambda _target: (_ for _ in ()).throw(RuntimeError("no transcript")),
        raising=False,
    )



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
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(
        cli,
        [
            "send",
            "gpt",
            "please review this",
            "--artifact",
            str(artifact),
        ],
    )

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["from"] == "claude"
    assert payload["to"] == "gpt"
    assert payload["artifact"] == str(artifact)
    assert payload["summary"] == "please review this"
    assert payload["injectStatus"] == "submitted"
    assert payload["turnObserved"] == "unavailable"
    assert payload["followUp"]["command"] == "hive doctor gpt"
    assert payload["path"].endswith(".json")
    assert len(sent) == 1
    assert sent == [f"<HIVE from=claude to=gpt id={FIXED_ID} artifact={artifact}>\nplease review this\n</HIVE>"]
    assert len(bus.read_all_events(workspace)) == 1
    assert bus.read_all_events(workspace)[0]["intent"] == "send"


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
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "test", "--wait"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "confirmed"
    assert "followUp" not in payload


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
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.adapters.base.wait_for_id_in_transcript", lambda path, message_id, baseline, timeout=45.0: False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "test", "--wait"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "unconfirmed"
    assert payload["followUp"]["command"] == "hive doctor gpt"
    assert "consider resending" in payload["followUp"]["afterDiagnosis"]


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
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "test"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "unavailable"
    assert payload["injectStatus"] == "submitted"
    assert payload["followUp"]["command"] == "hive doctor gpt"


def test_send_async_pending_includes_delivery_follow_up(runner, configure_hive_home, monkeypatch, tmp_path):
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
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.observer.fork_observer", lambda *args, **kwargs: 4321)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "test"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["turnObserved"] == "pending"
    assert payload["observerPid"] == 4321
    assert payload["followUp"]["command"] == f"hive delivery {FIXED_ID}"
    assert payload["followUp"]["suggestedAfterSec"] == 10
    assert payload["followUp"]["ifNotConfirmed"] == "run hive doctor gpt before considering resend"


def test_send_inject_failure_advises_doctor_without_observer(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

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

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "test"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["injectStatus"] == "failed"
    assert payload["turnObserved"] == "unavailable"
    assert "observerPid" not in payload
    assert payload["followUp"]["command"] == "hive doctor gpt"


# --- Send gate tests ---


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

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, transcript.stat().st_size), raising=False)
    monkeypatch.setattr("hive.adapters.base.wait_for_id_in_transcript", lambda path, message_id, baseline, timeout=45.0: False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    return workspace, transcript, sent


def test_send_blocked_by_gate(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
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

    result = runner.invoke(cli, ["send", "gpt", "hello"])

    assert result.exit_code != 0
    assert "waiting for a user answer" in result.output


def test_gate_fail_open_no_transcript(runner, configure_hive_home, monkeypatch, tmp_path):
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

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "hello"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["injectStatus"] == "submitted"
    assert payload["turnObserved"] == "unavailable"


# --- Answer command tests ---


def test_answer_when_not_waiting_fails(runner, configure_hive_home, monkeypatch, tmp_path):
    """answer should fail when the target is not in waiting state."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    transcript.write_text(
        json.dumps({"type": "user", "message": {"role": "user", "content": "hello"}}) + "\n"
    )

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"
        cli = "claude"

        def is_alive(self) -> bool:
            return True

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")

    result = runner.invoke(cli, ["answer", "gpt", "yes"])

    assert result.exit_code != 0
    assert "not waiting for an answer" in result.output


def test_answer_when_waiting_injects_text(runner, configure_hive_home, monkeypatch, tmp_path):
    """answer should inject text when the target is waiting."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    bus.init_workspace(workspace)

    transcript = tmp_path / "session.jsonl"
    transcript.write_text(
        json.dumps({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "proceed?"}},
                ],
            },
        }) + "\n"
    )

    injected: list[tuple[str, str]] = []

    class _FakeAgent:
        pane_id = "%99"
        name = "gpt"
        cli = "claude"

        def is_alive(self) -> bool:
            return True

    class _FakeTeam:
        def __init__(self):
            self.workspace = str(workspace)
            self.name = "team-x"
            self.tmux_session = "dev"
            self.tmux_window = "dev:0"

        def get(self, _name: str):
            return _FakeAgent()

    def fake_submit(pane_id, text, cli):
        injected.append((pane_id, text))
        # Simulate the answer being accepted — write a user turn.
        transcript.write_text(
            transcript.read_text()
            + json.dumps({"type": "user", "message": {"role": "user", "content": "yes"}}) + "\n"
        )

    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "orch")
    monkeypatch.setattr("hive.agent._submit_interactive_text", fake_submit)

    result = runner.invoke(cli, ["answer", "gpt", "yes"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["ack"] == "confirmed"
    assert payload["answer"] == "yes"
    assert payload["question"] == "proceed?"
    assert len(injected) == 1
    assert injected[0] == ("%99", "yes")
    # Event was written
    events = bus.read_all_events(workspace)
    assert len(events) == 1
    assert events[0]["intent"] == "answer"
