from __future__ import annotations

import json
from pathlib import Path

from hive import adapters
from hive.adapters.base import check_input_gate


def _write_session(root: Path, cwd: str, session_id: str, *, model: str = "grok-4.6") -> Path:
    from urllib.parse import quote

    group = root / "sessions" / quote(cwd, safe="")
    session_dir = group / session_id
    session_dir.mkdir(parents=True)
    (session_dir / "summary.json").write_text(json.dumps({
        "info": {"id": session_id, "cwd": cwd},
        "created_at": "2026-08-17T02:08:17.527174Z",
        "current_model_id": model,
        "generated_title": "hello",
    }) + "\n")
    updates = session_dir / "updates.jsonl"
    updates.write_text(
        json.dumps({
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": "hi"},
                },
            },
        }) + "\n"
    )
    return updates


def test_grok_read_meta_ignores_foreign_jsonl(tmp_path):
    path = tmp_path / "session.jsonl"
    path.write_text(json.dumps({
        "type": "message",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
    }) + "\n")
    assert adapters.get("grok").read_meta(path) is None


def test_grok_find_and_read_meta(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    path = _write_session(tmp_path, "/Users/notdp/work", "01abc")
    adapter = adapters.get("grok")
    assert adapter.find_session_file("01abc", cwd="/Users/notdp/work") == path
    meta = adapter.read_meta(path)
    assert meta is not None
    assert meta.session_id == "01abc"
    assert meta.cwd == "/Users/notdp/work"
    assert meta.model == "grok-4.6"
    assert meta.cli_name == "grok"


def test_grok_list_sessions_filters_cwd(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    _write_session(tmp_path, "/a", "sid-a")
    _write_session(tmp_path, "/b", "sid-b")
    adapter = adapters.get("grok")
    hits = list(adapter.list_sessions(cwd="/a"))
    assert [m.session_id for m in hits] == ["sid-a"]


def test_grok_iter_messages(tmp_path, monkeypatch):
    monkeypatch.setenv("GROK_HOME", str(tmp_path))
    path = _write_session(tmp_path, "/w", "sid")
    messages = list(adapters.get("grok").iter_messages(path))
    assert len(messages) == 1
    assert messages[0].role == "user"
    assert messages[0].parts[0].text == "hi"


def test_grok_input_gate_user_chunk_is_clear(tmp_path):
    path = tmp_path / "updates.jsonl"
    path.write_text(json.dumps({
        "method": "session/update",
        "params": {"update": {"sessionUpdate": "user_message_chunk", "content": {"text": "hi"}}},
    }) + "\n")
    result = check_input_gate(path)
    assert result.status == "clear"


def test_grok_input_gate_ask_is_waiting(tmp_path):
    path = tmp_path / "updates.jsonl"
    path.write_text(json.dumps({
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "tool_call",
                "title": "ask_user_question",
                "_meta": {"x.ai/tool": {"name": "ask_user_question"}},
            }
        },
    }) + "\n")
    result = check_input_gate(path)
    assert result.status == "waiting"
