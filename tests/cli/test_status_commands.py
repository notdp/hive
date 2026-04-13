import json

from hive import bus
from hive.cli import cli, _probe_member_input_state


def test_status_exposes_lead_session_id(runner, configure_hive_home, monkeypatch, tmp_path):
    configure_hive_home()
    monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": "orch-session-456")
    monkeypatch.setattr("hive.team.resolve_session_id_for_pane", lambda _pane: "orch-session-456")
    workspace = tmp_path / "ws"

    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0
    result = runner.invoke(cli, ["team"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["self"] == "orch"
    assert payload["tmuxWindow"] == "dev:0"
    orch = next(member for member in payload["members"] if member["name"] == "orch")
    assert orch["role"] == "agent"
    assert orch["sessionId"] == "orch-session-456"


def test_legacy_status_commands_show_migration_error(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-status", "--workspace", str(workspace)]).exit_code == 0

    status_set_result = runner.invoke(cli, ["status-set", "busy", "working", "--agent", "claude"])
    assert status_set_result.exit_code != 0
    assert "`hive status-set` was removed" in status_set_result.output
    assert "hive send" in status_set_result.output

    wait_result = runner.invoke(cli, ["wait-status", "claude", "--state", "done"])
    assert wait_result.exit_code != 0
    assert "`hive wait-status` was removed" in wait_result.output

    status_result = runner.invoke(cli, ["status", "--agent", "claude"])
    assert status_result.exit_code != 0
    assert "`hive status` was removed" in status_result.output
    assert "hive team" in status_result.output

    statuses_result = runner.invoke(cli, ["statuses"])
    assert statuses_result.exit_code != 0
    assert "`hive statuses` was removed" in statuses_result.output

    status_show_result = runner.invoke(cli, ["status-show"])
    assert status_show_result.exit_code != 0
    assert "`hive status-show` was removed" in status_show_result.output


# --- _probe_member_input_state tests ---


def test_probe_skips_non_agent_roles():
    member = {"name": "term-1", "role": "terminal", "alive": True, "pane": "%9"}
    _probe_member_input_state(member)
    assert "inputState" not in member


def test_probe_dead_agent_returns_offline():
    member = {"name": "claude", "role": "agent", "alive": False, "pane": "%9"}
    _probe_member_input_state(member)
    assert member["inputState"] == "offline"
    assert member["inputReason"] == "pane_dead"


def test_probe_no_profile_returns_unknown(monkeypatch):
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: None)
    member = {"name": "claude", "role": "agent", "alive": True, "pane": "%9"}
    _probe_member_input_state(member)
    assert member["inputState"] == "unknown"
    assert member["inputReason"] == "no_session"


def test_probe_waiting_user_sets_pending_question(monkeypatch, tmp_path):
    from hive.adapters.base import GateResult
    from hive.agent_cli import CLIProfile

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

    profile = CLIProfile(name="claude", ready_text="Claude Code", resume_cmd="", skill_cmd="/{name}")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: profile)

    class _FakeAdapter:
        name = "claude"
        def resolve_current_session_id(self, _pane):
            return "sess-123"
        def find_session_file(self, _sid, *, cwd=None):
            return transcript

    import hive.adapters
    monkeypatch.setattr(hive.adapters, "get", lambda _name: _FakeAdapter())
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp")

    member = {"name": "claude", "role": "agent", "alive": True, "pane": "%9"}
    _probe_member_input_state(member)
    assert member["inputState"] == "waiting_user"
    assert member["inputReason"] == "ask_pending"
    assert member["pendingQuestion"] == "proceed?"


def test_probe_clear_returns_ready(monkeypatch, tmp_path):
    from hive.adapters.base import GateResult
    from hive.agent_cli import CLIProfile

    transcript = tmp_path / "session.jsonl"
    transcript.write_text(
        json.dumps({"type": "user", "message": {"role": "user", "content": "hello"}}) + "\n"
    )

    profile = CLIProfile(name="claude", ready_text="Claude Code", resume_cmd="", skill_cmd="/{name}")
    monkeypatch.setattr("hive.cli.detect_profile_for_pane", lambda _pane: profile)

    class _FakeAdapter:
        name = "claude"
        def resolve_current_session_id(self, _pane):
            return "sess-123"
        def find_session_file(self, _sid, *, cwd=None):
            return transcript

    import hive.adapters
    monkeypatch.setattr(hive.adapters, "get", lambda _name: _FakeAdapter())
    monkeypatch.setattr("hive.cli.tmux.display_value", lambda _pane, _fmt: "/tmp")

    member = {"name": "claude", "role": "agent", "alive": True, "pane": "%9"}
    _probe_member_input_state(member)
    assert member["inputState"] == "ready"
    assert member["inputReason"] == ""


def test_team_includes_needs_answer(runner, configure_hive_home, monkeypatch, tmp_path):
    """hive team should include needsAnswer when agents are waiting."""
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-na", "--workspace", str(workspace)]).exit_code == 0

    # Mock _probe_member_input_state to simulate a waiting agent.
    def fake_probe(member):
        if member.get("name") == "orch":
            member["inputState"] = "waiting_user"
            member["inputReason"] = "ask_pending"
            member["pendingQuestion"] = "proceed?"

    monkeypatch.setattr("hive.cli._probe_member_input_state", fake_probe)

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["needsAnswer"] == ["orch"]


def test_who_includes_terminals(runner, configure_hive_home, tmp_path):
    configure_hive_home()
    workspace = tmp_path / "ws"
    assert runner.invoke(cli, ["create", "team-w", "--workspace", str(workspace)]).exit_code == 0
    assert runner.invoke(cli, ["terminal", "add", "term-1", "--pane", "%77"]).exit_code == 0

    result = runner.invoke(cli, ["team"])
    assert result.exit_code == 0
    payload = json.loads(result.output)
    terminal = next(member for member in payload["members"] if member["name"] == "term-1")
    assert terminal["role"] == "terminal"
