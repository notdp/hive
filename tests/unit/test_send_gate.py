from __future__ import annotations

import json
from pathlib import Path

import pytest

from hive.adapters.base import (
    GateResult,
    _is_assistant_ask,
    check_input_gate,
)


def _write_jsonl(path: Path, records: list[dict]) -> None:
    path.write_text("\n".join(json.dumps(r) for r in records) + "\n")


# --- _is_assistant_ask: claude format ---

def test_is_assistant_ask_claude():
    payload = {
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Let me ask you something."},
                {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "Continue?"}},
            ],
        },
    }
    assert _is_assistant_ask(payload) is True


# --- _is_assistant_ask: droid format ---

def test_is_assistant_ask_droid():
    payload = {
        "type": "message",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "OK?"}},
            ],
        },
    }
    assert _is_assistant_ask(payload) is True


# --- _is_assistant_ask: codex format ---

def test_is_assistant_ask_codex():
    payload = {
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": "AskUserQuestion",
            "arguments": '{"question": "proceed?"}',
        },
    }
    assert _is_assistant_ask(payload) is True


# --- rejects other tools ---

def test_is_assistant_ask_codex_request_user_input():
    """Codex uses request_user_input instead of AskUserQuestion."""
    payload = {
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": "request_user_input",
            "arguments": '{"prompt": "choose option"}',
        },
    }
    assert _is_assistant_ask(payload) is True


def test_rejects_other_tools():
    payload = {
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "name": "Read", "input": {"file_path": "/tmp/x"}},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
            ],
        },
    }
    assert _is_assistant_ask(payload) is False


def test_rejects_user_turn():
    payload = {"type": "user", "message": {"role": "user", "content": "hello"}}
    assert _is_assistant_ask(payload) is False


# --- check_input_gate: end-to-end on JSONL files ---

def test_waiting_when_ask_is_last(tmp_path):
    path = tmp_path / "session.jsonl"
    _write_jsonl(path, [
        {"type": "user", "message": {"role": "user", "content": "do something"}},
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "sure?"}},
                ],
            },
        },
    ])
    result = check_input_gate(path)
    assert result.status == "waiting"


def test_not_waiting_when_answered(tmp_path):
    path = tmp_path / "session.jsonl"
    _write_jsonl(path, [
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "sure?"}},
                ],
            },
        },
        {"type": "user", "message": {"role": "user", "content": "yes"}},
    ])
    result = check_input_gate(path)
    assert result.status == "clear"


def test_clear_after_codex_function_call_output(tmp_path):
    """Codex answers come as function_call_output, not user messages."""
    path = tmp_path / "session.jsonl"
    _write_jsonl(path, [
        {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "request_user_input",
                "call_id": "call_123",
                "arguments": '{"prompt": "choose"}',
            },
        },
        {
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_123",
                "output": "option A",
            },
        },
    ])
    result = check_input_gate(path)
    assert result.status == "clear"


def test_waiting_codex_request_user_input(tmp_path):
    """Codex request_user_input without answer should block."""
    path = tmp_path / "session.jsonl"
    _write_jsonl(path, [
        {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "request_user_input",
                "call_id": "call_456",
                "arguments": '{"prompt": "choose"}',
            },
        },
    ])
    result = check_input_gate(path)
    assert result.status == "waiting"


def test_clear_when_no_ask(tmp_path):
    path = tmp_path / "session.jsonl"
    _write_jsonl(path, [
        {"type": "user", "message": {"role": "user", "content": "hello"}},
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hi there"}],
            },
        },
        {"type": "user", "message": {"role": "user", "content": "thanks"}},
    ])
    result = check_input_gate(path)
    assert result.status == "clear"


def test_fail_open_missing_file(tmp_path):
    path = tmp_path / "nonexistent.jsonl"
    result = check_input_gate(path)
    assert result.status == "unknown"


def test_fail_open_empty_file(tmp_path):
    path = tmp_path / "empty.jsonl"
    path.write_text("")
    result = check_input_gate(path)
    assert result.status == "unknown"


def test_unicode_transcript_does_not_raise(tmp_path):
    """UTF-8 multibyte characters must not cause UnicodeDecodeError (fail-open)."""
    path = tmp_path / "session.jsonl"
    # Build a transcript >8KB with Chinese text so seek lands mid-character
    chinese_text = "你好世界" * 500  # ~6000 bytes of Chinese
    records = [
        {"type": "user", "message": {"role": "user", "content": chinese_text}},
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "继续吗？"}},
                ],
            },
        },
    ]
    path.write_text(
        "\n".join(json.dumps(r, ensure_ascii=False) for r in records) + "\n",
        encoding="utf-8",
    )
    result = check_input_gate(path)
    assert result.status in ("waiting", "unknown")  # must not raise


def test_expands_window_for_large_records(tmp_path):
    """When the last relevant record is larger than initial 8KB chunk,
    the function should expand the read window to find it."""
    path = tmp_path / "session.jsonl"
    # Write a user turn, then a large assistant ask that exceeds 8KB
    large_text = "x" * 12000
    records = [
        {"type": "user", "message": {"role": "user", "content": "start"}},
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": large_text},
                    {"type": "tool_use", "name": "AskUserQuestion", "input": {"question": "ok?"}},
                ],
            },
        },
    ]
    _write_jsonl(path, records)
    result = check_input_gate(path)
    assert result.status == "waiting"
