"""Step 2 coverage: find_session_file + read_meta + list_sessions."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from hive import adapters


# --- droid -------------------------------------------------------------------


def test_droid_find_session_file_by_glob(tmp_path, monkeypatch):
    monkeypatch.setenv("FACTORY_HOME", str(tmp_path))
    sessions_root = tmp_path / "sessions" / "-Users-notdp-Developer-demo"
    sessions_root.mkdir(parents=True)
    target = sessions_root / "abc-123.jsonl"
    target.write_text(
        json.dumps({"type": "session_start", "id": "abc-123", "cwd": "/Users/notdp/Developer/demo"}) + "\n"
    )

    adapter = adapters.get("droid")
    resolved = adapter.find_session_file("abc-123")
    assert resolved == target

    with_hint = adapter.find_session_file("abc-123", cwd="/Users/notdp/Developer/demo")
    assert with_hint == target


def test_droid_find_session_file_missing(tmp_path, monkeypatch):
    monkeypatch.setenv("FACTORY_HOME", str(tmp_path))
    (tmp_path / "sessions").mkdir()
    adapter = adapters.get("droid")
    assert adapter.find_session_file("does-not-exist") is None


def test_droid_read_meta_parses_session_start(tmp_path):
    path = tmp_path / "droid.jsonl"
    path.write_text(
        json.dumps(
            {
                "type": "session_start",
                "id": "sess-1",
                "title": "demo",
                "sessionTitle": "demo title",
                "cwd": "/work",
            }
        )
        + "\n"
        + json.dumps({"type": "message", "message": {"role": "user", "content": []}})
        + "\n"
    )
    adapter = adapters.get("droid")
    meta = adapter.read_meta(path)
    assert meta is not None
    assert meta.session_id == "sess-1"
    assert meta.cli_name == "droid"
    assert meta.cwd == "/work"
    assert meta.title == "demo title"
    assert meta.jsonl_path == path


def test_droid_read_meta_returns_none_for_bad_file(tmp_path):
    path = tmp_path / "broken.jsonl"
    path.write_text("not json\n")
    assert adapters.get("droid").read_meta(path) is None


def test_droid_list_sessions_filters_by_cwd(tmp_path, monkeypatch):
    monkeypatch.setenv("FACTORY_HOME", str(tmp_path))
    root = tmp_path / "sessions" / "-work-a"
    root.mkdir(parents=True)
    (root / "a.jsonl").write_text(
        json.dumps({"type": "session_start", "id": "a", "cwd": "/work/a"}) + "\n"
    )
    other = tmp_path / "sessions" / "-work-b"
    other.mkdir(parents=True)
    (other / "b.jsonl").write_text(
        json.dumps({"type": "session_start", "id": "b", "cwd": "/work/b"}) + "\n"
    )

    adapter = adapters.get("droid")
    hits = list(adapter.list_sessions(cwd="/work/a"))
    assert [m.session_id for m in hits] == ["a"]


# --- claude ------------------------------------------------------------------


def _write_claude_jsonl(path: Path, session_id: str, cwd: str) -> None:
    path.write_text(
        json.dumps(
            {
                "type": "user",
                "sessionId": session_id,
                "cwd": cwd,
                "parentUuid": None,
                "uuid": "uuid-1",
                "timestamp": "2026-04-02T05:27:52.478Z",
                "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            }
        )
        + "\n"
    )


def test_claude_find_session_file_uses_cwd_slug(tmp_path, monkeypatch):
    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    projects = tmp_path / "projects" / "-Users-notdp-Developer-hive"
    projects.mkdir(parents=True)
    target = projects / "cafe-babe.jsonl"
    _write_claude_jsonl(target, "cafe-babe", "/Users/notdp/Developer/hive")

    adapter = adapters.get("claude")
    resolved = adapter.find_session_file("cafe-babe", cwd="/Users/notdp/Developer/hive")
    assert resolved == target

    # Also resolves via rglob when no cwd hint.
    assert adapter.find_session_file("cafe-babe") == target


def test_claude_read_meta_scans_first_records(tmp_path):
    path = tmp_path / "claude.jsonl"
    path.write_text(
        json.dumps({"type": "permission-mode", "permissionMode": "bypass"}) + "\n"
        + json.dumps(
            {
                "type": "user",
                "sessionId": "sess-c",
                "cwd": "/work",
                "parentUuid": None,
                "uuid": "u1",
                "timestamp": "2026-04-02T05:27:52.478Z",
                "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            }
        )
        + "\n"
    )
    adapter = adapters.get("claude")
    meta = adapter.read_meta(path)
    assert meta is not None
    assert meta.session_id == "sess-c"
    assert meta.cwd == "/work"
    assert meta.started_at is not None


def test_claude_read_meta_missing_session_id_returns_none(tmp_path):
    path = tmp_path / "no-id.jsonl"
    path.write_text(json.dumps({"type": "permission-mode"}) + "\n")
    assert adapters.get("claude").read_meta(path) is None


# --- codex -------------------------------------------------------------------


def test_codex_find_session_file_ignores_cwd_hint(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    root = tmp_path / "sessions" / "2026" / "04" / "02"
    root.mkdir(parents=True)
    target = root / "rollout-2026-04-02T00-00-00-019d4864-462c-7d41-bbb1-b00b17cdd0b2.jsonl"
    target.write_text(
        json.dumps(
            {
                "type": "session_meta",
                "payload": {"id": "019d4864-462c-7d41-bbb1-b00b17cdd0b2", "cwd": "/any"},
            }
        )
        + "\n"
    )

    adapter = adapters.get("codex")
    resolved = adapter.find_session_file("019d4864-462c-7d41-bbb1-b00b17cdd0b2", cwd="/nowhere")
    assert resolved == target


def test_codex_read_meta_parses_session_meta(tmp_path):
    path = tmp_path / "rollout-2026-04-02T00-00-00-deadbeef-dead-beef-dead-beefdeadbeef.jsonl"
    path.write_text(
        json.dumps(
            {
                "timestamp": "2026-04-02T00:00:00.000Z",
                "type": "session_meta",
                "payload": {
                    "id": "deadbeef-dead-beef-dead-beefdeadbeef",
                    "cwd": "/work",
                },
            }
        )
        + "\n"
    )
    adapter = adapters.get("codex")
    meta = adapter.read_meta(path)
    assert meta is not None
    assert meta.session_id == "deadbeef-dead-beef-dead-beefdeadbeef"
    assert meta.cwd == "/work"
    assert meta.started_at is not None


def test_codex_read_meta_rejects_non_meta(tmp_path):
    path = tmp_path / "rollout.jsonl"
    path.write_text(json.dumps({"type": "response_item", "payload": {}}) + "\n")
    assert adapters.get("codex").read_meta(path) is None


def test_codex_list_sessions_walks_tree(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    for date, sid in (("2026/03/01", "a-a-a-a-a"), ("2026/03/02", "b-b-b-b-b")):
        root = tmp_path / "sessions" / date
        root.mkdir(parents=True)
        (root / f"rollout-{date.replace('/', '-')}-{sid}.jsonl").write_text(
            json.dumps({"type": "session_meta", "payload": {"id": sid, "cwd": "/work"}}) + "\n"
        )

    adapter = adapters.get("codex")
    hits = list(adapter.list_sessions(limit=None))
    assert {m.session_id for m in hits} == {"a-a-a-a-a", "b-b-b-b-b"}
    assert all(m.cli_name == "codex" for m in hits)
