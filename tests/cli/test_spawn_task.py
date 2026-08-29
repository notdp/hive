"""Tests for `hive spawn` post role-decouple.

`hive spawn <name>` keeps the plain pane-spawn contract; `--task <artifact>`
turns it into the atomic dispatch primitive: the member boots into
`/hive:hive` and the task artifact rides the first `<HIVE>` message.
`--workflow` is gone for good.
"""

import json

import pytest

from hive.cli import cli

pytestmark = pytest.mark.cli


class _FakeTeam:
    def __init__(self, workspace: str):
        self.name = "team-x"
        self.workspace = workspace
        self.tmux_session = "dev"
        self.tmux_window = "dev:0"

    def spawn(self, name: str, **kwargs):
        from types import SimpleNamespace

        self.spawn_calls = getattr(self, "spawn_calls", [])
        self.spawn_calls.append({"name": name, **kwargs})
        return SimpleNamespace(pane_id="%55", cli=kwargs.get("cli") or "claude")


def _setup(runner, configure_hive_home, monkeypatch, tmp_path, *, ready=True, dispatch_ok=True):
    configure_hive_home()
    workspace = tmp_path / "ws"
    workspace.mkdir(exist_ok=True)
    team = _FakeTeam(str(workspace))
    monkeypatch.setattr("hive.cli._resolve_scoped_team", lambda _t, required=True: ("team-x", team))
    monkeypatch.setattr("hive.cli._ensure_team_hived", lambda t, ws: 1)
    monkeypatch.setattr("hive.cli._resolve_sender", lambda _n: "orch")

    waits: list[dict] = []

    def fake_wait(workspace_arg, *, team_name, agents, **kw):
        waits.append({"workspace": workspace_arg, "team": team_name, "agents": set(agents)})
        return set() if ready else set(agents)

    monkeypatch.setattr("hive.cli._wait_for_peer_ready", fake_wait)

    sends: list[dict] = []

    def fake_send(**kwargs):
        if not dispatch_ok:
            raise RuntimeError("transport refused")
        sends.append(kwargs)
        return {"msgId": "m1"}

    monkeypatch.setattr("hive.cli._request_send_payload", fake_send)

    task = tmp_path / "task.md"
    task.write_text("# scope\ndo the thing\n")
    return team, waits, sends, task


def test_spawn_task_dispatches_atomically(runner, configure_hive_home, monkeypatch, tmp_path):
    team, waits, sends, task = _setup(runner, configure_hive_home, monkeypatch, tmp_path)

    result = runner.invoke(cli, ["spawn", "explore", "--cli", "codex", "--task", str(task)])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload == {
        "agent": "explore",
        "pane": "%55",
        "task": str(task.resolve()),
        "dispatched": True,
    }
    # member boots straight into the member-contract plugin skill
    spawn_call = team.spawn_calls[0]
    assert spawn_call["prompt"] == ""
    assert spawn_call["skill"] == "hive:hive"
    # ready gate ran for exactly this member before the send (TUI-injected CLI)
    assert waits == [{"workspace": team.workspace, "team": "team-x", "agents": {"explore"}}]
    (send,) = sends
    assert send["sender_agent"] == "orch"
    assert send["target_agent"] == "explore"
    assert send["artifact"] == str(task.resolve())
    assert send["command_name"] == "spawn-dispatch"


def test_spawn_task_claude_skips_ready_gate(runner, configure_hive_home, monkeypatch, tmp_path):
    """A claude member's inbox queues: the task dispatches immediately after
    spawn, no inputState=ready wait."""
    team, waits, sends, task = _setup(runner, configure_hive_home, monkeypatch, tmp_path)

    result = runner.invoke(cli, ["spawn", "explore", "--cli", "claude", "--task", str(task)])
    assert result.exit_code == 0, result.output
    assert waits == []
    (send,) = sends
    assert send["target_agent"] == "explore"


def test_spawn_task_ready_timeout_fails_with_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    _team, _waits, sends, task = _setup(
        runner, configure_hive_home, monkeypatch, tmp_path, ready=False
    )

    result = runner.invoke(cli, ["spawn", "explore", "--cli", "codex", "--task", str(task)])
    assert result.exit_code == 1
    payload = json.loads(result.output)
    assert payload["status"] == "spawn_ready_timeout"
    assert payload["agent"] == "explore" and payload["pane"] == "%55"
    assert sends == []  # never dispatch into a pane that is not ready


def test_spawn_task_dispatch_failure_reports_retry_hint(runner, configure_hive_home, monkeypatch, tmp_path):
    _team, _waits, _sends, task = _setup(
        runner, configure_hive_home, monkeypatch, tmp_path, dispatch_ok=False
    )

    result = runner.invoke(cli, ["spawn", "explore", "--task", str(task)])
    assert result.exit_code == 1
    payload = json.loads(result.output)
    assert payload["status"] == "dispatch_failed"
    assert "transport refused" in payload["error"]
    assert "hive send explore" in payload["hint"]


def test_spawn_task_and_prompt_are_mutually_exclusive(runner, configure_hive_home, monkeypatch, tmp_path):
    team, waits, sends, task = _setup(runner, configure_hive_home, monkeypatch, tmp_path)

    result = runner.invoke(cli, ["spawn", "explore", "--task", str(task), "--prompt", "hi"])
    assert result.exit_code == 1
    assert "mutually exclusive" in result.output
    assert not getattr(team, "spawn_calls", [])
    assert waits == [] and sends == []


def test_spawn_task_requires_existing_artifact(runner, configure_hive_home, monkeypatch, tmp_path):
    _setup(runner, configure_hive_home, monkeypatch, tmp_path)

    result = runner.invoke(cli, ["spawn", "explore", "--task", str(tmp_path / "nope.md")])
    assert result.exit_code == 2  # Click path validation


def test_spawn_workflow_option_is_gone(runner, configure_hive_home, monkeypatch, tmp_path):
    _setup(runner, configure_hive_home, monkeypatch, tmp_path)

    result = runner.invoke(cli, ["spawn", "explore", "--workflow", "demo"])
    assert result.exit_code == 2
    assert "No such option" in result.output


def test_plain_spawn_contract_unchanged(runner, configure_hive_home, monkeypatch, tmp_path):
    team, waits, sends, _task = _setup(runner, configure_hive_home, monkeypatch, tmp_path)

    result = runner.invoke(cli, ["spawn", "dodo", "--prompt", "hello", "--skill", "none"])
    assert result.exit_code == 0, result.output
    assert "spawned in pane %55" in result.output
    spawn_call = team.spawn_calls[0]
    assert spawn_call["prompt"] == "hello" and spawn_call["skill"] == "none"
    assert waits == [] and sends == []  # no ready gate, no dispatch


def test_spawn_rejects_a_model_outside_the_cli_catalog(runner, configure_hive_home, monkeypatch, tmp_path):
    team, waits, sends, task = _setup(runner, configure_hive_home, monkeypatch, tmp_path)
    codex_home = tmp_path / "codex-home"
    codex_home.mkdir()
    (codex_home / "models_cache.json").write_text(
        json.dumps({"models": [{"slug": "gpt-5.6-sol"}, {"slug": "gpt-5.5"}]})
    )
    monkeypatch.setenv("CODEX_HOME", str(codex_home))

    result = runner.invoke(cli, ["spawn", "impl", "--cli", "codex", "-m", "gpt-5.6-soul"])
    assert result.exit_code == 1
    assert "gpt-5.6-soul" in result.output and "gpt-5.6-sol" in result.output
    assert not getattr(team, "spawn_calls", [])  # refused before any pane exists
