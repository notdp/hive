"""Smoke tests for session adapter registry + resolve_current_session_id."""

from __future__ import annotations

import json
import os

from hive import adapters, tmux


def test_registry_has_known_adapters():
    assert set(adapters.available()) == {"claude", "codex"}
    for name in ("claude", "codex"):
        adapter = adapters.get(name)
        assert adapter is not None
        assert isinstance(adapter, adapters.SessionAdapter)
        assert adapter.name == name


def test_get_unknown_adapter_returns_none():
    assert adapters.get("gemini") is None
    assert adapters.get("") is None


def test_claude_adapter_reads_pidfile_session(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    sessions_dir.mkdir()
    (sessions_dir / "98989.json").write_text(json.dumps({"sessionId": "sess-claude"}))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="98989", command="claude", argv="claude --verbose"),
    ])

    adapter = adapters.get("claude")
    assert adapter.resolve_current_session_id("%138") == "sess-claude"


def test_claude_adapter_reads_pidfile_when_claude_runs_under_node(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    sessions_dir.mkdir()
    (sessions_dir / "99907.json").write_text(json.dumps({"sessionId": "sess-claude-node"}))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(
            pid="99907",
            command="node",
            argv="node /opt/homebrew/bin/claude --verbose --resume 74e0fe8d-3278-436a-98f1-7dd32c817571",
        ),
    ])

    adapter = adapters.get("claude")
    assert adapter.resolve_current_session_id("%1070") == "sess-claude-node"


def test_claude_adapter_reads_pidfile_when_argv_is_claude_exe(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    sessions_dir.mkdir()
    (sessions_dir / "20473.json").write_text(json.dumps({"sessionId": "sess-claude-exe"}))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(
            pid="20473",
            command="/opt/homebrew/li",
            argv="/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe --resume 2315094b-1bac-4719-9111-2ef29b04d43a",
        ),
    ])

    adapter = adapters.get("claude")
    assert adapter.resolve_current_session_id("%479") == "sess-claude-exe"


def test_claude_adapter_returns_pidfile_session_for_matched_pid(monkeypatch, tmp_path):
    """Each PID reads its own pidfile; other transcripts in the same project
    directory do not interfere."""
    sessions_dir = tmp_path / "sessions"
    sessions_dir.mkdir()
    (sessions_dir / "42424.json").write_text(json.dumps({"sessionId": "sess-own"}))
    (sessions_dir / "55555.json").write_text(json.dumps({"sessionId": "sess-other"}))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="42424", command="claude", argv="claude --verbose"),
    ])

    adapter = adapters.get("claude")
    assert adapter.resolve_current_session_id("%1070") == "sess-own"


def test_claude_adapter_returns_none_when_no_pidfile(monkeypatch, tmp_path):
    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="43434", command="claude", argv="claude --verbose"),
    ])

    adapter = adapters.get("claude")
    assert adapter.resolve_current_session_id("%1070") is None


def test_claude_validated_pidfile_returns_session_when_transcript_is_current(monkeypatch, tmp_path):
    from hive.adapters.claude import resolve_session_id_from_pidfile

    claude_home = tmp_path / "claude-home"
    sessions_dir = claude_home / "sessions"
    projects_dir = claude_home / "projects" / "-repo"
    sessions_dir.mkdir(parents=True)
    projects_dir.mkdir(parents=True)
    (sessions_dir / "43434.json").write_text(json.dumps({
        "sessionId": "sess-idle",
        "updatedAt": 1_700_000_120_000,
        "cwd": "/repo",
    }))
    transcript = projects_dir / "sess-idle.jsonl"
    transcript.write_text(json.dumps({"sessionId": "sess-idle", "cwd": "/repo"}) + "\n")
    os.utime(transcript, (1_700_000_000.0, 1_700_000_000.0))

    monkeypatch.setenv("CLAUDE_HOME", str(claude_home))

    assert resolve_session_id_from_pidfile("43434", cwd="/repo") == "sess-idle"


def test_claude_validated_pidfile_rejects_stale_session(monkeypatch, tmp_path):
    from hive.adapters.claude import resolve_session_id_from_pidfile

    claude_home = tmp_path / "claude-home"
    sessions_dir = claude_home / "sessions"
    projects_dir = claude_home / "projects" / "-repo"
    sessions_dir.mkdir(parents=True)
    projects_dir.mkdir(parents=True)
    (sessions_dir / "43434.json").write_text(json.dumps({
        "sessionId": "sess-stale",
        "updatedAt": 1_700_001_000_000,
        "cwd": "/repo",
    }))
    transcript = projects_dir / "sess-stale.jsonl"
    transcript.write_text(json.dumps({"sessionId": "sess-stale", "cwd": "/repo"}) + "\n")
    os.utime(transcript, (1_700_000_000.0, 1_700_000_000.0))

    monkeypatch.setenv("CLAUDE_HOME", str(claude_home))

    assert resolve_session_id_from_pidfile("43434", cwd="/repo") is None


def test_codex_adapter_returns_none_without_daemon(monkeypatch, configure_hive_home):
    """Embedded codex is unsupported: no daemon means no session, and the
    resolver must not go guessing via the pane's tty processes."""
    configure_hive_home()
    monkeypatch.setattr(
        "hive.adapters.codex_app_server.session_id_for_pane", lambda _pane: None
    )

    adapter = adapters.get("codex")
    assert adapter.resolve_current_session_id("%141") is None
