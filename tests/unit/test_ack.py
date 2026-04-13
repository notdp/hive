from __future__ import annotations

import json
import threading
import time
from pathlib import Path

import pytest

from hive.adapters import base as base_module


def _write_jsonl(path: Path, lines: list[dict]) -> None:
    path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")


def _get_transcript_baseline(path: Path) -> int:
    return getattr(base_module, "get_transcript_baseline")(path)


def _is_user_turn(payload: dict) -> bool:
    return getattr(base_module, "_is_user_turn")(payload)


def _wait_for_nonce_in_transcript(path: Path, nonce: str, baseline: int, timeout: float = 45.0) -> bool:
    return getattr(base_module, "wait_for_nonce_in_transcript")(path, nonce, baseline, timeout=timeout)


def test_get_transcript_baseline_returns_file_size(tmp_path):
    path = tmp_path / "session.jsonl"
    path.write_text('{"type":"message"}\n')

    assert _get_transcript_baseline(path) == path.stat().st_size


def test_get_transcript_baseline_missing_file(tmp_path):
    assert _get_transcript_baseline(tmp_path / "missing.jsonl") == 0


def test_is_user_turn_droid():
    payload = {
        "type": "message",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        },
    }

    assert _is_user_turn(payload) is True


def test_is_user_turn_claude():
    payload = {
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "hello"}],
        },
    }

    assert _is_user_turn(payload) is True


def test_is_user_turn_codex():
    payload = {
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}],
        },
    }

    assert _is_user_turn(payload) is True


@pytest.mark.parametrize(
    "payload",
    [
        {
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "nonce-a1b2"}],
            },
        },
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "nonce-a1b2"}],
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "nonce-a1b2"}],
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call-1",
                "output": "nonce-a1b2",
            },
        },
    ],
)
def test_is_user_turn_rejects_assistant_turn(payload):
    assert _is_user_turn(payload) is False


def test_wait_finds_nonce_after_baseline(tmp_path):
    path = tmp_path / "session.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "before"}],
                },
            }
        ],
    )
    baseline = path.stat().st_size

    def writer() -> None:
        time.sleep(0.1)
        with path.open("a") as handle:
            handle.write(
                json.dumps(
                    {
                        "type": "message",
                        "message": {
                            "role": "user",
                            "content": [{"type": "text", "text": "nonce-a1b2 hello"}],
                        },
                    }
                )
                + "\n"
            )

    thread = threading.Thread(target=writer, daemon=True)
    thread.start()
    try:
        assert _wait_for_nonce_in_transcript(path, "nonce-a1b2", baseline, timeout=1.0) is True
    finally:
        thread.join(timeout=1.0)


def test_wait_times_out(tmp_path):
    path = tmp_path / "session.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "before"}],
                },
            }
        ],
    )
    baseline = path.stat().st_size

    assert _wait_for_nonce_in_transcript(path, "nonce-a1b2", baseline, timeout=0.2) is False


def test_wait_ignores_before_baseline(tmp_path):
    path = tmp_path / "session.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "nonce-a1b2 before"}],
                },
            }
        ],
    )
    baseline = path.stat().st_size

    assert _wait_for_nonce_in_transcript(path, "nonce-a1b2", baseline, timeout=0.2) is False


def test_wait_handles_partial_line(tmp_path):
    path = tmp_path / "session.jsonl"
    path.write_text("")
    baseline = path.stat().st_size

    payload = {
        "type": "message",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "nonce-a1b2 partial"}],
        },
    }
    encoded = json.dumps(payload)
    split_at = len(encoded) // 2

    def writer() -> None:
        with path.open("a") as handle:
            handle.write(encoded[:split_at])
            handle.flush()
            time.sleep(0.1)
            handle.write(encoded[split_at:] + "\n")
            handle.flush()

    thread = threading.Thread(target=writer, daemon=True)
    thread.start()
    try:
        assert _wait_for_nonce_in_transcript(path, "nonce-a1b2", baseline, timeout=1.0) is True
    finally:
        thread.join(timeout=1.0)


def test_wait_file_appears_during_poll(tmp_path):
    path = tmp_path / "session.jsonl"
    baseline = 0

    def writer() -> None:
        time.sleep(0.1)
        _write_jsonl(
            path,
            [
                {
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "nonce-a1b2 hello"}],
                    },
                }
            ],
        )

    thread = threading.Thread(target=writer, daemon=True)
    thread.start()
    try:
        assert _wait_for_nonce_in_transcript(path, "nonce-a1b2", baseline, timeout=1.0) is True
    finally:
        thread.join(timeout=1.0)
