import json
import os

from hive import agent_cli, tmux


def test_normalize_command_strips_path_and_aliases():
    assert agent_cli.normalize_command("droid") == "droid"
    assert agent_cli.normalize_command("/usr/local/bin/claude") == "claude"
    assert agent_cli.normalize_command("claude-code") == "claude"
    assert agent_cli.normalize_command("CODEX") == "codex"
    assert agent_cli.normalize_command("") == ""


def test_member_role_classifies_agents_and_shells():
    assert agent_cli.member_role("droid") == "agent"
    assert agent_cli.member_role("claude") == "agent"
    assert agent_cli.member_role("codex") == "agent"
    assert agent_cli.member_role("zsh") == "terminal"
    assert agent_cli.member_role("python3") == "terminal"


def test_profiles_use_expected_skill_commands():
    assert agent_cli.get_profile("droid").skill_cmd == "/{name}"
    assert agent_cli.get_profile("claude").skill_cmd == "/{name}"
    assert agent_cli.get_profile("codex").skill_cmd == "${name}"


def test_detect_profile_for_pane_uses_title_and_tty_processes(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "2.1.89")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "\u2733 Claude Code")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    profile = agent_cli.detect_profile_for_pane("%138")

    assert profile is not None
    assert profile.name == "claude"


def test_detect_profile_for_pane_falls_back_to_tty_processes(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "2.1.89")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="100", command="-zsh", argv="-zsh"),
        tmux.TTYProcessInfo(pid="200", command="codex", argv="codex"),
    ])

    profile = agent_cli.detect_profile_for_pane("%141")

    assert profile is not None
    assert profile.name == "codex"


def test_resolve_session_id_for_pane_dispatches_to_adapter(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    calls: list[str] = []

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            calls.append(pane_id)
            return "fake-sess"

    monkeypatch.setattr("hive.agent_cli.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    assert agent_cli.resolve_session_id_for_pane("%138") == "fake-sess"
    assert calls == ["%138"]


def test_resolve_session_id_for_pane_resolves_newer_claude_project_session(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    projects_dir = tmp_path / "projects" / "-repo"
    sessions_dir.mkdir(parents=True)
    projects_dir.mkdir(parents=True)
    (sessions_dir / "42424.json").write_text(json.dumps({"sessionId": "sess-old"}))

    stale = projects_dir / "sess-old.jsonl"
    stale.write_text(json.dumps({"sessionId": "sess-old", "cwd": "/repo"}) + "\n")
    fresh = projects_dir / "sess-new.jsonl"
    fresh.write_text(json.dumps({"sessionId": "sess-new", "cwd": "/repo"}) + "\n")
    stale_ns = 1_700_000_000_000_000_000
    fresh_ns = stale_ns + 5_000
    os.utime(stale, ns=(stale_ns, stale_ns))
    os.utime(fresh, ns=(fresh_ns, fresh_ns))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.display_value", lambda _pane, _fmt: "/repo")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="42424", command="claude", argv="claude --verbose"),
    ])

    assert agent_cli.resolve_session_id_for_pane("%138") == "sess-new"


def test_resolve_session_id_for_pane_returns_none_when_no_profile(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "zsh")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.resolve_session_id_for_pane("%2") is None


def test_member_role_for_pane_returns_agent_when_profile_detected(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "droid")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.member_role_for_pane("%1") == "agent"


def test_member_role_for_pane_returns_terminal_for_shell(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "zsh")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.member_role_for_pane("%2") == "terminal"
