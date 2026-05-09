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
