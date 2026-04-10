from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PAYLOAD_BUILDER = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin" / "droid-vim-payload"


def _build_payload(tmp_path: Path, *, orig: str, edited: str, mode: str, offset: int = 0) -> str:
    orig_file = tmp_path / "orig.md"
    edited_file = tmp_path / "edited.md"
    send_file = tmp_path / "send.txt"
    orig_file.write_text(orig)
    edited_file.write_text(edited)
    if offset:
        (tmp_path / "offset").write_text(str(offset))
    subprocess.run(
        [sys.executable, str(PAYLOAD_BUILDER), str(orig_file), str(edited_file), str(send_file), mode],
        check=True,
    )
    return send_file.read_text()


def test_diff_payload_builder_uses_shared_protocol_target_and_note(tmp_path):
    payload = _build_payload(tmp_path, orig="旧内容\n", edited="旧内容\n\n貌似可以了哦\n", mode="diff")

    assert "<droid_edit mode=\"diff\">" in payload
    assert "<edit_target>previous_assistant_message</edit_target>" in payload
    assert "紧邻上一条 assistant message" in payload
    assert "--- before" in payload
    assert "+++ after" in payload


def test_text_payload_builder_uses_shared_protocol_note(tmp_path):
    payload = _build_payload(tmp_path, orig="", edited="整理后的正文\n", mode="text")

    assert "<droid_edit mode=\"text\">" in payload
    assert "用户在外部编辑器里整理后的最新正文" in payload
    assert "<edited_text>\n整理后的正文\n</edited_text>" in payload


def test_diff_payload_with_offset_uses_indexed_target(tmp_path):
    payload = _build_payload(tmp_path, orig="旧内容\n", edited="旧内容\n\n新增\n", mode="diff", offset=1)

    assert "<edit_target>assistant_message[-2]</edit_target>" in payload
    assert "倒数第 2 条" in payload


def test_diff_payload_with_offset_2_uses_indexed_target(tmp_path):
    payload = _build_payload(tmp_path, orig="旧内容\n", edited="旧内容\n\n新增\n", mode="diff", offset=2)

    assert "<edit_target>assistant_message[-3]</edit_target>" in payload
    assert "倒数第 3 条" in payload


def test_diff_payload_uses_effective_offset_from_transcript(tmp_path):
    transcript = tmp_path / "codex.jsonl"
    transcript.write_text(
        "".join(
            json.dumps(row) + "\n"
            for row in [
                {"type": "session_meta", "payload": {"id": "sess-codex", "cwd": "/repo"}},
                {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-1"}},
                {
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "phase": "final_answer",
                        "content": [{"type": "output_text", "text": "真正要编辑的回答"}],
                    },
                },
                {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-2"}},
                {
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "$cvim"}],
                    },
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "phase": "commentary",
                        "content": [{"type": "output_text", "text": "使用 `cvim` skill，按要求直接启动外部编辑器助手。"}],
                    },
                },
            ]
        )
    )
    (tmp_path / "transcript_path").write_text(str(transcript))

    payload = _build_payload(tmp_path, orig="真正要编辑的回答\n", edited="真正要编辑的回答\n\n补一行\n", mode="diff")

    assert "<edit_target>assistant_message[-2]</edit_target>" in payload
    assert "倒数第 2 条" in payload
