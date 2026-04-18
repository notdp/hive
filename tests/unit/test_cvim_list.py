"""Tests for `list_recent_assistant_messages` and the `cvim-list` helper.

The helper produces the payload the vim popup reads: a JSON array of
`{offset, label}` entries plus one seed file per offset. Offsets match
`extract_last_assistant_text(offset=N)`, so 0 is the newest assistant
message visible to the user.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
LIST_HELPER = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin" / "cvim-list"
SHARED_DIR = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin"


def _import_shared():
    if str(SHARED_DIR) not in sys.path:
        sys.path.insert(0, str(SHARED_DIR))
    import _cvim_shared
    return _cvim_shared


def _write_jsonl(path: Path, rows: list[dict[str, object]]) -> None:
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))


def test_list_recent_returns_newest_first_with_offset(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "claude.jsonl"
    _write_jsonl(transcript, [
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "first"}]}},
        {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}},
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "second"}]}},
        {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}},
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "third"}]}},
    ])
    entries = shared.list_recent_assistant_messages(transcript, limit=10)
    assert [e["offset"] for e in entries] == [0, 1, 2]
    assert [e["text"] for e in entries] == ["third", "second", "first"]


def test_list_recent_skips_empty_assistant_messages(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "claude.jsonl"
    _write_jsonl(transcript, [
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "real"}]}},
        {"type": "message", "message": {"role": "assistant", "content": []}},
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "   "}]}},
    ])
    entries = shared.list_recent_assistant_messages(transcript, limit=10)
    assert [e["text"] for e in entries] == ["real"]


def test_list_recent_respects_limit(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "claude.jsonl"
    rows = []
    for idx in range(25):
        rows.append({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": f"msg-{idx}"}]}})
    _write_jsonl(transcript, rows)
    entries = shared.list_recent_assistant_messages(transcript, limit=10)
    assert len(entries) == 10
    assert entries[0]["text"] == "msg-24"
    assert entries[-1]["text"] == "msg-15"


def test_list_recent_preview_truncates_to_80_chars(tmp_path):
    shared = _import_shared()
    long_line = "x" * 200
    transcript = tmp_path / "claude.jsonl"
    _write_jsonl(transcript, [
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": long_line}]}},
    ])
    entries = shared.list_recent_assistant_messages(transcript, limit=10)
    assert len(entries) == 1
    assert entries[0]["preview"].endswith("…")
    assert len(entries[0]["preview"]) == 80


def test_list_recent_preview_uses_first_non_empty_line(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "claude.jsonl"
    _write_jsonl(transcript, [
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "\n\n   \nhello world\nsecond"}]}},
    ])
    entries = shared.list_recent_assistant_messages(transcript, limit=10)
    assert entries[0]["preview"] == "hello world"


def test_list_recent_extracts_timestamp_hhmm(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "claude.jsonl"
    _write_jsonl(transcript, [
        {
            "type": "assistant",
            "sessionId": "s",
            "uuid": "u",
            "timestamp": "2025-01-01T12:34:56Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
        },
    ])
    entries = shared.list_recent_assistant_messages(transcript, limit=10)
    assert len(entries) == 1
    assert entries[0]["timestamp"] and len(entries[0]["timestamp"]) == 5
    assert entries[0]["timestamp"][2] == ":"


def test_list_recent_codex_reads_output_text_messages(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "codex.jsonl"
    _write_jsonl(transcript, [
        {"type": "session_meta", "payload": {"id": "sess-codex", "cwd": "/repo"}},
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "codex answer"}],
            },
        },
    ])
    entries = shared.list_recent_assistant_messages(transcript, limit=10)
    assert [e["text"] for e in entries] == ["codex answer"]


def test_list_recent_handles_missing_file(tmp_path):
    shared = _import_shared()
    entries = shared.list_recent_assistant_messages(tmp_path / "missing.jsonl", limit=10)
    assert entries == []


def _run_list_helper(transcript: Path, workdir: Path, *, limit: int | None = None) -> tuple[Path, Path, list[dict[str, object]]]:
    seeds_dir = workdir / "seeds"
    menu_json = workdir / "menu.json"
    args = [sys.executable, str(LIST_HELPER), str(transcript), str(seeds_dir), str(menu_json)]
    if limit is not None:
        args.append(str(limit))
    subprocess.run(args, check=True, env=os.environ.copy())
    return seeds_dir, menu_json, json.loads(menu_json.read_text())


def test_cvim_list_writes_seeds_and_menu_json(tmp_path):
    transcript = tmp_path / "claude.jsonl"
    _write_jsonl(transcript, [
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "first"}]}},
        {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}},
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "second"}]}},
        {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}},
        {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "third"}]}},
    ])
    seeds_dir, menu_json, menu = _run_list_helper(transcript, tmp_path)

    assert [entry["offset"] for entry in menu] == [0, 1, 2]
    assert "third" in menu[0]["label"]
    assert (seeds_dir / "0.md").read_text().rstrip("\n") == "third"
    assert (seeds_dir / "1.md").read_text().rstrip("\n") == "second"
    assert (seeds_dir / "2.md").read_text().rstrip("\n") == "first"


def test_cvim_list_respects_limit_arg(tmp_path):
    transcript = tmp_path / "claude.jsonl"
    rows = []
    for idx in range(5):
        rows.append({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": f"m{idx}"}]}})
    _write_jsonl(transcript, rows)
    _seeds_dir, _menu_json, menu = _run_list_helper(transcript, tmp_path, limit=3)
    assert len(menu) == 3
    assert menu[0]["offset"] == 0


def test_cvim_list_handles_empty_transcript(tmp_path):
    transcript = tmp_path / "empty.jsonl"
    transcript.write_text("")
    _seeds_dir, menu_json, menu = _run_list_helper(transcript, tmp_path)
    assert menu == []
    assert menu_json.read_text() == "[]"
