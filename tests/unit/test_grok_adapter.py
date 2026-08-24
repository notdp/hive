"""Grok adapter coverage: on-disk layout, meta, and record normalization."""

from __future__ import annotations

import json
import os
from pathlib import Path
from urllib.parse import quote

import pytest

from hive import adapters


CWD = "/Users/dp/work/hive"
OTHER_CWD = "/tmp/other"


@pytest.fixture
def grok_home(tmp_path, monkeypatch) -> Path:
    home = tmp_path / ".grok"
    monkeypatch.setenv("GROK_HOME", str(home))
    return home


def _write_session(home: Path, session_id: str, cwd: str, records: list[dict]) -> Path:
    session_dir = home / "sessions" / quote(cwd, safe="") / session_id
    session_dir.mkdir(parents=True)
    history = session_dir / "chat_history.jsonl"
    history.write_text("".join(json.dumps(record) + "\n" for record in records))
    return history


def _assistant(text: str = "ok", model_id: str = "grok-4.6-build") -> dict:
    return {"type": "assistant", "content": text, "model_id": model_id}


# --- discovery ---------------------------------------------------------------


def test_find_session_file_uses_quoted_cwd_directory(grok_home):
    target = _write_session(grok_home, "sess-a", CWD, [_assistant()])
    _write_session(grok_home, "sess-b", OTHER_CWD, [_assistant()])

    adapter = adapters.get("grok")
    assert adapter.find_session_file("sess-a", cwd=CWD) == target


def test_find_session_file_globs_when_cwd_is_unknown_or_wrong(grok_home):
    target = _write_session(grok_home, "sess-a", CWD, [_assistant()])

    adapter = adapters.get("grok")
    assert adapter.find_session_file("sess-a") == target
    assert adapter.find_session_file("sess-a", cwd="/nowhere") == target


def test_find_session_file_returns_none_when_missing(grok_home):
    _write_session(grok_home, "sess-a", CWD, [_assistant()])

    adapter = adapters.get("grok")
    assert adapter.find_session_file("sess-missing") is None
    assert adapter.find_session_file("") is None


def test_list_sessions_orders_by_mtime_and_filters_by_cwd(grok_home):
    old = _write_session(grok_home, "sess-old", CWD, [_assistant()])
    new = _write_session(grok_home, "sess-new", OTHER_CWD, [_assistant()])
    os.utime(old, (1_700_000_000.0, 1_700_000_000.0))
    os.utime(new, (1_700_000_500.0, 1_700_000_500.0))

    adapter = adapters.get("grok")
    assert [m.session_id for m in adapter.list_sessions()] == ["sess-new", "sess-old"]
    assert [m.cwd for m in adapter.list_sessions(cwd=CWD)] == [CWD]
    assert [m.session_id for m in adapter.list_sessions(limit=1)] == ["sess-new"]
    assert all(m.cli_name == "grok" for m in adapter.list_sessions())


def test_resolve_current_session_id_delegates_to_leader(monkeypatch):
    seen: list[str] = []

    def _session_id_for_pane(pane_id: str) -> str | None:
        seen.append(pane_id)
        return "sess-from-leader"

    monkeypatch.setattr(
        "hive.adapters.grok_leader.session_id_for_pane", _session_id_for_pane
    )

    adapter = adapters.get("grok")
    assert adapter.resolve_current_session_id("%42") == "sess-from-leader"
    assert seen == ["%42"]


# --- meta --------------------------------------------------------------------


def test_read_meta_prefers_summary_json(grok_home):
    history = _write_session(grok_home, "sess-a", CWD, [_assistant(model_id="grok-4.6-build")])
    (history.parent / "summary.json").write_text(
        json.dumps({"title": "nonce hunt", "model": "grok-4.6", "timestamp": "2026-08-23T18:12:34.567640+00:00"})
    )

    meta = adapters.get("grok").read_meta(history)
    assert meta is not None
    assert meta.session_id == "sess-a"
    assert meta.cwd == CWD
    assert meta.model == "grok-4.6"
    assert meta.title == "nonce hunt"
    assert meta.started_at is not None and meta.started_at.year == 2026
    assert meta.jsonl_path == history


def test_read_meta_falls_back_to_mtime_and_first_assistant_model(grok_home):
    history = _write_session(
        grok_home,
        "sess-b",
        CWD,
        [
            {"type": "system", "content": "You are Grok 4.6."},
            {"type": "user", "content": [{"type": "text", "text": "hi"}]},
            _assistant(model_id="grok-4.6-build"),
        ],
    )
    os.utime(history, (1_700_000_000.0, 1_700_000_000.0))

    meta = adapters.get("grok").read_meta(history)
    assert meta is not None
    assert meta.model == "grok-4.6-build"
    assert meta.title is None
    assert meta.started_at is not None
    assert meta.started_at.timestamp() == 1_700_000_000.0


def test_read_meta_rejects_other_files(grok_home, tmp_path):
    stray = tmp_path / "rollout.jsonl"
    stray.write_text(json.dumps({"type": "assistant", "content": "hi"}) + "\n")
    assert adapters.get("grok").read_meta(stray) is None


# --- messages ----------------------------------------------------------------


def test_iter_messages_maps_every_record_type(grok_home):
    history = _write_session(
        grok_home,
        "sess-c",
        CWD,
        [
            {"type": "system", "content": "You are Grok 4.6."},
            {"type": "user", "content": [{"type": "text", "text": "<user_query>\nhi\n</user_query>"}]},
            {"type": "reasoning", "content": [{"type": "text", "text": "thinking hard"}]},
            {"type": "tool_result", "tool_name": "read_file", "content": [{"type": "text", "text": "file body"}]},
            _assistant(text="NONCE-7q3x"),
            {"type": "rewind_marker", "content": "ignored"},
        ],
    )

    messages = list(adapters.get("grok").iter_messages(history))
    assert [m.role for m in messages] == ["system", "user", "assistant", "tool", "assistant"]
    assert [m.parts[0].kind for m in messages] == [
        "text",
        "text",
        "thinking",
        "tool_result",
        "text",
    ]
    assert messages[1].parts[0].text == "<user_query>\nhi\n</user_query>"
    assert messages[2].parts[0].text == "thinking hard"
    assert messages[3].parts[0].tool_name == "read_file"
    assert messages[3].parts[0].tool_output == "file body"
    assert messages[4].parts[0].text == "NONCE-7q3x"
    assert messages[4].raw["model_id"] == "grok-4.6-build"


def test_message_from_record_handles_list_assistant_content_and_unknowns():
    adapter = adapters.get("grok")
    listed = adapter.message_from_record(
        {"type": "assistant", "content": [{"type": "text", "text": "a"}, {"type": "image", "url": "x"}]}
    )
    assert listed is not None
    assert listed.role == "assistant"
    assert [p.kind for p in listed.parts] == ["text", "unknown"]

    assert adapter.message_from_record({"type": "rewind_marker"}) is None
    assert adapter.message_from_record({}) is None


def test_iter_messages_missing_file_yields_nothing(tmp_path):
    assert list(adapters.get("grok").iter_messages(tmp_path / "nope.jsonl")) == []
