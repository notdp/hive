import json
import os

from hive import agent_cli, tmux


def test_normalize_command_strips_path_and_aliases():
    assert agent_cli.normalize_command("droid") == "droid"
    assert agent_cli.normalize_command("/usr/local/bin/claude") == "claude"
    assert agent_cli.normalize_command("claude-code") == "claude"
    assert agent_cli.normalize_command("CODEX") == "codex"
    assert agent_cli.normalize_command("claude.exe") == "claude"
    assert agent_cli.normalize_command("/opt/homebrew/bin/claude.exe") == "claude"
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


def test_detect_profile_for_pane_claude_exe_not_misled_by_codex_title(monkeypatch):
    # Regression: macOS Claude Code reports comm "claude.exe". The command probe
    # must resolve it to claude so detection never falls back to the pane title,
    # which would misclassify a claude pane whose title mentions another CLI.
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude.exe")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "✳ Research Codex app server")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    profile = agent_cli.detect_profile_for_pane("%1")

    assert profile is not None
    assert profile.name == "claude"


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


def test_resolve_session_id_for_pane_returns_pidfile_session(monkeypatch, tmp_path):
    """Adapter reads the PID-anchored pidfile to resolve the session."""
    sessions_dir = tmp_path / "sessions"
    sessions_dir.mkdir()
    (sessions_dir / "42424.json").write_text(json.dumps({"sessionId": "sess-pid"}))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="42424", command="claude", argv="claude --verbose"),
    ])

    assert agent_cli.resolve_session_id_for_pane("%138") == "sess-pid"


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
