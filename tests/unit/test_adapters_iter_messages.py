"""Step 3 coverage: iter_messages normalization across CLIs."""

from __future__ import annotations

import json
from pathlib import Path

from hive import adapters


def _write_jsonl(path: Path, lines: list[dict]) -> None:
    path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")


# --- droid -------------------------------------------------------------------


def test_droid_iter_messages_normalizes_text_thinking_tool_use(tmp_path):
    path = tmp_path / "droid.jsonl"
    _write_jsonl(
        path,
        [
            {"type": "session_start", "id": "s", "cwd": "/w"},
            {
                "type": "message",
                "id": "m1",
                "parentId": None,
                "timestamp": "2026-04-02T05:27:52.478Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}],
                },
            },
            {
                "type": "message",
                "id": "m2",
                "parentId": "m1",
                "timestamp": "2026-04-02T05:27:53.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "hmm"},
                        {"type": "text", "text": "world"},
                        {"type": "tool_use", "name": "Grep", "input": {"pattern": "x"}},
                    ],
                },
            },
            {
                "type": "message",
                "id": "m3",
                "parentId": "m2",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "content": [{"type": "text", "text": "match"}],
                        }
                    ],
                },
            },
        ],
    )

    messages = list(adapters.get("droid").iter_messages(path))
    assert [m.role for m in messages] == ["user", "assistant", "user"]
    assert messages[0].parts[0].kind == "text"
    assert messages[0].parts[0].text == "hello"

    kinds = [p.kind for p in messages[1].parts]
    assert kinds == ["thinking", "text", "tool_use"]
    assert messages[1].parts[0].text == "hmm"
    assert messages[1].parts[2].tool_name == "Grep"
    assert messages[1].parts[2].tool_input == {"pattern": "x"}

    assert messages[2].parts[0].kind == "tool_result"
    assert messages[2].parts[0].tool_output == "match"
    assert messages[1].parent_id == "m1"


def test_droid_iter_messages_skips_non_message_lines(tmp_path):
    path = tmp_path / "droid.jsonl"
    _write_jsonl(
        path,
        [
            {"type": "session_start", "id": "s"},
            {"type": "file-history-snapshot"},
            {"type": "message", "message": {"role": "user", "content": "plain text"}},
        ],
    )
    messages = list(adapters.get("droid").iter_messages(path))
    assert len(messages) == 1
    assert messages[0].parts[0].kind == "text"
    assert messages[0].parts[0].text == "plain text"


# --- claude ------------------------------------------------------------------


def test_claude_iter_messages_normalizes_text_and_tool_use(tmp_path):
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "user",
                "uuid": "u1",
                "parentUuid": None,
                "sessionId": "s",
                "cwd": "/w",
                "timestamp": "2026-04-02T05:27:52.478Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "hi"}],
                },
            },
            {
                "type": "assistant",
                "uuid": "u2",
                "parentUuid": "u1",
                "timestamp": "2026-04-02T05:27:53.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "hmm"},
                        {"type": "text", "text": "ok"},
                        {"type": "tool_use", "name": "Read", "input": {"path": "/a"}},
                    ],
                },
            },
        ],
    )

    messages = list(adapters.get("claude").iter_messages(path))
    assert len(messages) == 2
    assert messages[0].role == "user"
    assert messages[0].parts[0].text == "hi"
    assert messages[1].role == "assistant"
    assert [p.kind for p in messages[1].parts] == ["thinking", "text", "tool_use"]
    assert messages[1].parts[2].tool_name == "Read"
    assert messages[1].parent_id == "u1"


def test_claude_iter_messages_handles_string_content(tmp_path):
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {
                "type": "user",
                "uuid": "u1",
                "sessionId": "s",
                "message": {"role": "user", "content": "plain"},
            },
        ],
    )
    messages = list(adapters.get("claude").iter_messages(path))
    assert len(messages) == 1
    assert messages[0].parts[0].kind == "text"
    assert messages[0].parts[0].text == "plain"


def test_claude_iter_messages_skips_unknown_record_types(tmp_path):
    path = tmp_path / "claude.jsonl"
    _write_jsonl(
        path,
        [
            {"type": "permission-mode", "permissionMode": "bypass"},
            {"type": "file-history-snapshot", "messageId": "x"},
            {
                "type": "assistant",
                "uuid": "u1",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            },
        ],
    )
    messages = list(adapters.get("claude").iter_messages(path))
    assert len(messages) == 1


# --- codex -------------------------------------------------------------------


def test_codex_iter_messages_normalizes_message_reasoning_function_call(tmp_path):
    path = tmp_path / "codex.jsonl"
    _write_jsonl(
        path,
        [
            {"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}},
            {
                "type": "response_item",
                "timestamp": "2026-04-02T05:27:52.478Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}],
                },
            },
            {
                "type": "response_item",
                "timestamp": "2026-04-02T05:27:53.000Z",
                "payload": {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "plan"}],
                },
            },
            {
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}],
                },
            },
            {
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "call_id": "call-1",
                    "arguments": json.dumps({"cmd": "ls"}),
                },
            },
            {
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": "a\nb",
                },
            },
        ],
    )

    messages = list(adapters.get("codex").iter_messages(path))
    assert [m.role for m in messages] == ["user", "assistant", "assistant", "assistant", "tool"]

    assert messages[0].parts[0].kind == "text"
    assert messages[0].parts[0].text == "hi"

    assert messages[1].parts[0].kind == "thinking"
    assert messages[1].parts[0].text == "plan"

    assert messages[2].parts[0].text == "done"

    assert messages[3].parts[0].kind == "tool_use"
    assert messages[3].parts[0].tool_name == "shell"
    assert messages[3].parts[0].tool_input == {"cmd": "ls"}
    assert messages[3].message_id == "call-1"

    assert messages[4].parts[0].kind == "tool_result"
    assert messages[4].parts[0].tool_output == "a\nb"
    assert messages[4].message_id == "call-1"


def test_codex_iter_messages_unknown_item_becomes_unknown_part(tmp_path):
    path = tmp_path / "codex.jsonl"
    _write_jsonl(
        path,
        [
            {"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}},
            {
                "type": "response_item",
                "payload": {"type": "some_future_item", "detail": 42},
            },
        ],
    )
    messages = list(adapters.get("codex").iter_messages(path))
    assert len(messages) == 1
    assert messages[0].role == "unknown"
    assert messages[0].parts[0].kind == "unknown"
    assert messages[0].parts[0].raw == {"type": "some_future_item", "detail": 42}


def test_codex_iter_messages_normalizes_custom_tool_calls(tmp_path):
    path = tmp_path / "codex-custom.jsonl"
    _write_jsonl(
        path,
        [
            {"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}},
            {
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "apply_patch",
                    "call_id": "call-2",
                    "arguments": {"patch": "..."},
                },
            },
            {
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "call-2",
                    "output": {"text": "done"},
                },
            },
        ],
    )

    messages = list(adapters.get("codex").iter_messages(path))
    assert [m.role for m in messages] == ["assistant", "tool"]
    assert messages[0].parts[0].kind == "tool_use"
    assert messages[0].parts[0].tool_name == "apply_patch"
    assert messages[0].parts[0].tool_input == {"patch": "..."}
    assert messages[0].message_id == "call-2"
    assert messages[1].parts[0].kind == "tool_result"
    assert messages[1].parts[0].tool_output == "done"
    assert messages[1].message_id == "call-2"


# --- cross-CLI parity --------------------------------------------------------


def test_all_adapters_return_messages_with_uniform_shape(tmp_path):
    """Regardless of CLI, every Message yields parts with .kind and .role in expected set."""
    droid_path = tmp_path / "droid.jsonl"
    _write_jsonl(
        droid_path,
        [
            {"type": "session_start", "id": "s"},
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}},
        ],
    )
    claude_path = tmp_path / "claude.jsonl"
    _write_jsonl(
        claude_path,
        [{"type": "user", "uuid": "u1", "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}],
    )
    codex_path = tmp_path / "codex.jsonl"
    _write_jsonl(
        codex_path,
        [
            {"type": "session_meta", "payload": {"id": "s", "cwd": "/w"}},
            {
                "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            },
        ],
    )

    allowed_kinds = {"text", "thinking", "tool_use", "tool_result", "image", "unknown"}

    for name, path in (("droid", droid_path), ("claude", claude_path), ("codex", codex_path)):
        msgs = list(adapters.get(name).iter_messages(path))
        assert msgs, f"{name} yielded no messages"
        for msg in msgs:
            assert msg.role in {"user", "assistant", "system", "developer", "tool", "unknown"}
            for part in msg.parts:
                assert part.kind in allowed_kinds
