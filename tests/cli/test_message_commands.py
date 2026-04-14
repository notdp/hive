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
    assert "summary" not in payload
    assert payload["state"] == "unavailable"
    assert "injectStatus" not in payload
    assert "turnObserved" not in payload
    assert "followUp" not in payload
    assert payload["path"].endswith(".json")
    assert len(sent) == 1
    assert payload["msgId"] == FIXED_ID
    assert sent == [f"<HIVE from=claude to=gpt msgId={FIXED_ID} artifact={artifact}>\nplease review this\n</HIVE>"]
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
    assert payload["state"] == "confirmed"
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
    assert payload["state"] == "unconfirmed"
    assert "followUp" not in payload


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
    assert payload["state"] == "unavailable"
    assert "injectStatus" not in payload
    assert "followUp" not in payload


def test_send_async_pending_enqueues_sidecar(runner, configure_hive_home, monkeypatch, tmp_path):
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

    enqueued = []
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr(
        "hive.sidecar.detect_runtime_queue_state",
        lambda **_kw: {"state": "not_queued", "source": "capture", "observedAt": "2026-04-14T00:00:00Z"},
    )
    monkeypatch.setattr("hive.sidecar.enqueue_pending", lambda *a, **kw: enqueued.append(a))
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "test"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["state"] == "pending"
    assert "runtimeQueueState" not in payload
    assert "followUp" not in payload
    assert len(enqueued) == 1  # sidecar was asked to track this message


def test_send_async_queued_reports_runtime_queue_state(runner, configure_hive_home, monkeypatch, tmp_path):
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

    enqueued: list[tuple[tuple[object, ...], dict[str, object]]] = []
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _team, required=True: ("team-x", _FakeTeam()))
    monkeypatch.setattr("hive.cli.secrets.token_urlsafe", lambda _n=4: FIXED_ID)
    monkeypatch.setattr("hive.cli._resolve_ack_baseline", lambda _target: (transcript, 0), raising=False)
    monkeypatch.setattr(
        "hive.sidecar.detect_runtime_queue_state",
        lambda **_kw: {"state": "queued", "source": "capture", "observedAt": "2026-04-14T00:00:00Z"},
    )
    monkeypatch.setattr("hive.sidecar.enqueue_pending", lambda *a, **kw: enqueued.append((a, kw)))
    monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *a, **kw: 4321)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _from_agent=None: "claude")

    result = runner.invoke(cli, ["send", "gpt", "test"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["state"] == "queued"
    assert "runtimeQueueState" not in payload
    assert len(enqueued) == 1
    assert enqueued[0][1]["runtime_queue_state"] == "queued"
    assert enqueued[0][1]["queue_source"] == "capture"
    assert enqueued[0][1]["queue_probe_text"] == "test"

    send_events = [event for event in bus.read_all_events(workspace) if event.get("intent") == "send"]
    assert len(send_events) == 1
    assert send_events[0]["runtimeQueueState"] == "queued"
    assert send_events[0]["queueSource"] == "capture"
    assert send_events[0]["msgId"] == FIXED_ID


def test_send_inject_failure_no_sidecar(runner, configure_hive_home, monkeypatch, tmp_path):
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
    assert payload["state"] == "failed"
    assert "injectStatus" not in payload
    assert "turnObserved" not in payload
    assert "observerPid" not in payload
    assert "followUp" not in payload


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
    assert payload["state"] == "unavailable"
    assert "injectStatus" not in payload


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
