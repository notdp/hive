from __future__ import annotations

import json
from pathlib import Path

from hive import adapters


def _write_jsonl(path: Path, lines: list[dict]) -> None:
    path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")


def test_extract_context_snapshot_reads_latest_assistant_usage(tmp_path):
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "assistant",
                "timestamp": "2026-05-09T01:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "usage": {
                        "input_tokens": 10,
                        "cache_creation_input_tokens": 20,
                        "cache_read_input_tokens": 30,
                    },
                },
            },
            {
                "type": "assistant",
                "timestamp": "2026-05-09T01:02:03.000Z",
                "message": {
                    "role": "assistant",
                    "usage": {
                        "input_tokens": 1000,
                        "cache_creation_input_tokens": 200,
                        "cache_read_input_tokens": 300,
                        "output_tokens": 40,
                    },
                },
            },
            {
                "type": "user",
                "timestamp": "2026-05-09T01:02:04.000Z",
                "message": {"role": "user", "content": "next"},
            },
        ],
    )

    snapshot = adapters.get("claude").extract_context_snapshot(path)

    assert snapshot is not None
    assert snapshot.tokens == 1500
    assert snapshot.window is None
    assert snapshot.observed_at is not None
    assert snapshot.observed_at.isoformat() == "2026-05-09T01:02:03+00:00"
    assert snapshot.source == "claude_assistant_usage"


def test_extract_context_snapshot_returns_none_after_compact_until_new_turn(tmp_path):
    # Reproduces the window between /compact completing and the next
    # assistant turn arriving. The pre-compact assistant usage must not
    # leak through, because it reflects the old (huge) context size.
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "assistant",
                "timestamp": "2026-05-13T16:49:28.335Z",
                "message": {
                    "role": "assistant",
                    "usage": {
                        "input_tokens": 6,
                        "cache_creation_input_tokens": 680,
                        "cache_read_input_tokens": 531376,
                    },
                },
            },
            {
                "type": "user",
                "timestamp": "2026-05-13T16:52:05.969Z",
                "message": {"role": "user", "content": "summary text"},
                "isCompactSummary": True,
            },
        ],
    )

    snapshot = adapters.get("claude").extract_context_snapshot(path)

    assert snapshot is None


def test_extract_context_snapshot_uses_post_compact_assistant_turn(tmp_path):
    # Once a new assistant turn lands after the compact summary, reverse
    # iteration hits it first and reports the new (small) usage.
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "assistant",
                "timestamp": "2026-05-13T16:49:28.335Z",
                "message": {
                    "role": "assistant",
                    "usage": {
                        "input_tokens": 6,
                        "cache_creation_input_tokens": 680,
                        "cache_read_input_tokens": 531376,
                    },
                },
            },
            {
                "type": "user",
                "timestamp": "2026-05-13T16:52:05.969Z",
                "message": {"role": "user", "content": "summary text"},
                "isCompactSummary": True,
            },
            {
                "type": "assistant",
                "timestamp": "2026-05-14T03:54:52.718Z",
                "message": {
                    "role": "assistant",
                    "usage": {
                        "input_tokens": 1,
                        "cache_creation_input_tokens": 1294,
                        "cache_read_input_tokens": 78060,
                    },
                },
            },
        ],
    )

    snapshot = adapters.get("claude").extract_context_snapshot(path)

    assert snapshot is not None
    assert snapshot.tokens == 79355
    assert snapshot.observed_at is not None
    assert snapshot.observed_at.isoformat() == "2026-05-14T03:54:52.718000+00:00"


def test_extract_context_snapshot_maps_opus_4_7_to_1m_window(tmp_path):
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "assistant",
                "timestamp": "2026-05-14T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-4-7",
                    "usage": {
                        "input_tokens": 1,
                        "cache_read_input_tokens": 100_000,
                    },
                },
            },
        ],
    )

    snapshot = adapters.get("claude").extract_context_snapshot(path)

    assert snapshot is not None
    assert snapshot.window == 1_000_000


def test_extract_context_snapshot_maps_opus_4_8_to_1m_window(tmp_path):
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "assistant",
                "timestamp": "2026-05-14T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "model": "claude-opus-4-8",
                    "usage": {
                        "input_tokens": 1,
                        "cache_read_input_tokens": 100_000,
                    },
                },
            },
        ],
    )

    snapshot = adapters.get("claude").extract_context_snapshot(path)

    assert snapshot is not None
    assert snapshot.window == 1_000_000


def test_extract_context_snapshot_unknown_model_window_is_none(tmp_path):
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "assistant",
                "timestamp": "2026-05-14T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "model": "claude-sonnet-4-6",
                    "usage": {
                        "input_tokens": 1,
                        "cache_read_input_tokens": 100_000,
                    },
                },
            },
        ],
    )

    snapshot = adapters.get("claude").extract_context_snapshot(path)

    assert snapshot is not None
    assert snapshot.window is None
