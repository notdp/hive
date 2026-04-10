"""Smoke tests for session adapter registry + resolve_current_session_id."""

from __future__ import annotations

import json

from hive import adapters, tmux


def test_registry_has_three_known_adapters():
    assert set(adapters.available()) == {"droid", "claude", "codex"}
    for name in ("droid", "claude", "codex"):
        adapter = adapters.get(name)
        assert adapter is not None
        assert isinstance(adapter, adapters.SessionAdapter)
        assert adapter.name == name


def test_get_unknown_adapter_returns_none():
    assert adapters.get("gemini") is None
    assert adapters.get("") is None


def test_droid_adapter_resolves_via_session_map(tmp_path, monkeypatch, configure_hive_home):
    configure_hive_home()
    from hive import core_hooks

    session_map = core_hooks.session_map_path()
    session_map.parent.mkdir(parents=True, exist_ok=True)
    session_map.write_text(json.dumps({
        "by_pane": {"%10": {"session_id": "sess-droid"}},
        "by_tty": {},
        "by_pid": {},
    }))

    monkeypatch.setattr("hive.adapters.droid.tmux.get_pane_tty", lambda _pane: "/dev/ttys100")

    adapter = adapters.get("droid")
    assert adapter.resolve_current_session_id("%10") == "sess-droid"


def test_claude_adapter_reads_pid_mapping(monkeypatch):
    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        sessions_dir = root / "sessions"
        sessions_dir.mkdir(parents=True)
        (sessions_dir / "98989.json").write_text(json.dumps({"sessionId": "sess-claude"}))

        monkeypatch.setenv("CLAUDE_HOME", str(root))
        monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
        monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
            tmux.TTYProcessInfo(pid="98989", command="claude", argv="claude --verbose"),
        ])

        adapter = adapters.get("claude")
        assert adapter.resolve_current_session_id("%138") == "sess-claude"


def test_claude_adapter_reads_pid_mapping_when_claude_runs_under_node(monkeypatch):
    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        sessions_dir = root / "sessions"
        sessions_dir.mkdir(parents=True)
        (sessions_dir / "99907.json").write_text(json.dumps({"sessionId": "sess-claude-node"}))

        monkeypatch.setenv("CLAUDE_HOME", str(root))
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


def test_codex_adapter_resolves_via_lsof(monkeypatch, configure_hive_home):
    configure_hive_home()
    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        sessions_dir = root / "sessions"
        sessions_dir.mkdir(parents=True)
        jsonl_name = "rollout-2026-04-01T17-33-44-019d4864-462c-7d41-bbb1-b00b17cdd0b2.jsonl"
        (sessions_dir / jsonl_name).write_text("")

        monkeypatch.setenv("CODEX_HOME", str(root))
        monkeypatch.setattr("hive.adapters.codex.tmux.get_pane_tty", lambda _pane: "/dev/ttys015")
        monkeypatch.setattr("hive.adapters.codex.tmux.list_tty_processes", lambda _tty: [
            tmux.TTYProcessInfo(pid="5555", command="codex", argv="codex"),
        ])
        monkeypatch.setattr("hive.adapters.codex.tmux.list_open_files", lambda _pid: [
            str(sessions_dir / jsonl_name),
        ])
        monkeypatch.setattr("hive.adapters.codex.tmux.display_value", lambda _pane, _fmt: "/work")

        adapter = adapters.get("codex")
        assert adapter.resolve_current_session_id("%141") == "019d4864-462c-7d41-bbb1-b00b17cdd0b2"


def test_codex_adapter_scans_by_cwd_when_lsof_empty(monkeypatch, configure_hive_home):
    configure_hive_home()
    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        sessions_dir = root / "sessions" / "sub"
        sessions_dir.mkdir(parents=True)
        (sessions_dir / "rollout.jsonl").write_text(
            json.dumps({"type": "session_meta", "payload": {"id": "sess-jsonl", "cwd": "/work"}}) + "\n"
        )

        monkeypatch.setenv("CODEX_HOME", str(root))
        monkeypatch.setattr("hive.adapters.codex.tmux.get_pane_tty", lambda _pane: "/dev/ttys015")
        monkeypatch.setattr("hive.adapters.codex.tmux.list_tty_processes", lambda _tty: [])
        monkeypatch.setattr("hive.adapters.codex.tmux.display_value", lambda _pane, _fmt: "/work")

        adapter = adapters.get("codex")
        assert adapter.resolve_current_session_id("%141") == "sess-jsonl"
