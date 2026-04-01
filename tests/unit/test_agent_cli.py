import json

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


def test_resolve_session_id_for_pane_reads_claude_pid_file(tmp_path, monkeypatch):
    sessions_dir = tmp_path / ".claude" / "sessions"
    sessions_dir.mkdir(parents=True)
    (sessions_dir / "98989.json").write_text(json.dumps({"sessionId": "sess-claude"}))

    monkeypatch.setattr("hive.agent_cli.Path.home", lambda: tmp_path)
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "2.1.89")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "\u2733 Claude Code")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="98989", command="claude", argv="claude --verbose"),
    ])

    session_id = agent_cli.resolve_session_id_for_pane("%138")
    assert session_id == "sess-claude"


def test_resolve_codex_session_id_prefers_session_map(tmp_path, monkeypatch, configure_hive_home):
    configure_hive_home()
    from hive import core_hooks

    session_map = core_hooks.session_map_path()
    session_map.parent.mkdir(parents=True, exist_ok=True)
    session_map.write_text(json.dumps({
        "by_pane": {"%141": {"session_id": "sess-from-map"}},
        "by_tty": {},
        "by_pid": {},
    }))

    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "codex")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys015")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    session_id = agent_cli.resolve_session_id_for_pane("%141")
    assert session_id == "sess-from-map"


def test_resolve_codex_session_id_via_lsof(tmp_path, monkeypatch, configure_hive_home):
    configure_hive_home()

    sessions_dir = tmp_path / ".codex" / "sessions"
    sessions_dir.mkdir(parents=True)
    jsonl_name = "rollout-2026-04-01T17-33-44-019d4864-462c-7d41-bbb1-b00b17cdd0b2.jsonl"
    (sessions_dir / jsonl_name).write_text("")

    monkeypatch.setattr("hive.agent_cli.Path.home", lambda: tmp_path)
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "codex")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys015")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="5555", command="codex", argv="codex"),
    ])
    monkeypatch.setattr("hive.agent_cli.tmux.list_open_files", lambda _pid: [
        str(sessions_dir / jsonl_name),
    ])

    session_id = agent_cli.resolve_session_id_for_pane("%141")
    assert session_id == "019d4864-462c-7d41-bbb1-b00b17cdd0b2"


def test_resolve_codex_session_id_falls_back_to_jsonl(tmp_path, monkeypatch, configure_hive_home):
    configure_hive_home()

    sessions_dir = tmp_path / ".codex" / "sessions" / "sub"
    sessions_dir.mkdir(parents=True)
    (sessions_dir / "session.jsonl").write_text(
        json.dumps({"type": "session_meta", "payload": {"id": "sess-jsonl", "cwd": "/work"}}) + "\n"
    )

    monkeypatch.setattr("hive.agent_cli.Path.home", lambda: tmp_path)
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "codex")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys015")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])
    monkeypatch.setattr("hive.agent_cli.tmux.display_value", lambda _pane, _fmt: "/work")

    session_id = agent_cli.resolve_session_id_for_pane("%141")
    assert session_id == "sess-jsonl"


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
