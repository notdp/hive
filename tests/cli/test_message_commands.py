import json

from hive.cli import cli


def test_send_injects_hive_envelope_into_target_pane(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    artifact = tmp_path / "review.md"
    artifact.write_text("review request")
    for name in ("artifacts", "status", "presence", "state"):
        (workspace / name).mkdir(parents=True, exist_ok=True)

    sent: list[str] = []

    class _FakeAgent:
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
    assert result.output.strip() == "Sent HIVE message to gpt."
    assert sent == [f"<HIVE from=claude to=gpt artifact={artifact}>\nplease review this\n</HIVE>"]


def test_send_requires_tmux(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: False)

    result = runner.invoke(cli, ["send", "gpt", "hello from current context"])

    assert result.exit_code != 0
    assert "requires tmux" in result.output


def test_send_requires_live_registered_agent(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    for name in ("artifacts", "status", "presence", "state"):
        (workspace / name).mkdir(parents=True, exist_ok=True)

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


def test_type_alias_still_works(runner, configure_hive_home, monkeypatch):
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

    result = runner.invoke(cli, ["type", "claude", "plain prompt"])
    assert result.exit_code == 0
    assert sent == ["plain prompt"]


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


def test_notify_uses_current_pane_by_default(runner, monkeypatch):
    monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: "%72")
    monkeypatch.setattr(
        "hive.cli.notify_ui.notify",
        lambda message, pane_id, seconds, highlight, window_status, native_banner: {
            "message": message,
            "paneId": pane_id,
            "seconds": seconds,
            "highlight": highlight,
            "windowStatus": window_status,
            "nativeBanner": native_banner,
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
        "nativeBanner": True,
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
