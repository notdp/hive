from __future__ import annotations

import json
from pathlib import Path

from hive import adapters


def _write_jsonl(path: Path, lines: list[dict]) -> None:
    path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")


def test_extract_context_snapshot_reads_latest_token_count(tmp_path):
    path = tmp_path / "codex.jsonl"
    _write_jsonl(
        path,
        [
            {
                "timestamp": "2026-05-09T08:15:09.429Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 225016,
                            "cached_input_tokens": 222080,
                            "output_tokens": 914,
                            "reasoning_output_tokens": 376,
                            "total_tokens": 225930,
                        },
                        "model_context_window": 258400,
                    },
                },
            },
            {
                "timestamp": "2026-05-09T08:16:36.978Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 0,
                            "cached_input_tokens": 0,
                            "output_tokens": 0,
                            "reasoning_output_tokens": 0,
                            "total_tokens": 24028,
                        },
                        "model_context_window": 258400,
                    },
                },
            },
            {
                "timestamp": "2026-05-09T08:16:37.000Z",
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": []},
            },
        ],
    )

    snapshot = adapters.get("codex").extract_context_snapshot(path)

    assert snapshot is not None
    assert snapshot.tokens == 24028
    assert snapshot.window == 258400
    assert snapshot.observed_at is not None
    assert snapshot.observed_at.isoformat() == "2026-05-09T08:16:36.978000+00:00"
    assert snapshot.source == "codex_token_count_event"
