from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
PAYLOAD_BUILDER = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin" / "cvim-payload"


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


def test_diff_payload_uses_comment_wrapper_with_default_target(tmp_path):
    payload = _build_payload(tmp_path, orig="旧内容\n", edited="旧内容\n\n貌似可以了哦\n", mode="diff")

    assert payload.startswith('<comment on="previous_reply">')
    assert payload.endswith("</comment>")
    assert "--- " not in payload
    assert "+++ " not in payload
    assert "@@" in payload


def test_text_payload_is_bare_pass_through(tmp_path):
    payload = _build_payload(tmp_path, orig="", edited="整理后的正文\n", mode="text")

    assert payload == "整理后的正文"
    assert "<comment" not in payload


def test_diff_payload_with_offset_targets_indexed_reply(tmp_path):
    payload = _build_payload(tmp_path, orig="旧内容\n", edited="旧内容\n\n新增\n", mode="diff", offset=1)

    assert payload.startswith('<comment on="reply[-2]">')


def test_diff_payload_with_offset_2_targets_indexed_reply(tmp_path):
    payload = _build_payload(tmp_path, orig="旧内容\n", edited="旧内容\n\n新增\n", mode="diff", offset=2)

    assert payload.startswith('<comment on="reply[-3]">')


def test_diff_payload_menu_selected_skips_codex_commentary_shift(tmp_path):
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
                        "content": [{"type": "output_text", "text": "使用 cvim skill 启动外部编辑器。"}],
                    },
                },
            ]
        )
    )
    (tmp_path / "transcript_path").write_text(str(transcript))
    (tmp_path / "menu_selected").write_text("1")

    payload = _build_payload(tmp_path, orig="a\n", edited="a\nb\n", mode="diff", offset=1)

    assert payload.startswith('<comment on="reply[-2]">')


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
                        "content": [{"type": "output_text", "text": "使用 `cvim` skill,按要求直接启动外部编辑器助手。"}],
                    },
                },
            ]
        )
    )
    (tmp_path / "transcript_path").write_text(str(transcript))

    payload = _build_payload(tmp_path, orig="真正要编辑的回答\n", edited="真正要编辑的回答\n\n补一行\n", mode="diff")

    assert payload.startswith('<comment on="reply[-2]">')
