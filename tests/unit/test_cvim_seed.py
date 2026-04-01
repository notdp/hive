import json
import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SEED_HELPER = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin" / "droid-vim-seed"


def _write_jsonl(path: Path, rows: list[dict[str, object]]) -> None:
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))


def _run_seed_helper(
    tmp_path: Path,
    *,
    cwd: str,
    preferred: Path | None = None,
    factory_home: Path | None = None,
    offset: int = 0,
) -> str:
    dst = tmp_path / "seed.txt"
    env = os.environ.copy()
    if factory_home is not None:
        env["FACTORY_HOME"] = str(factory_home)
    args = [
        sys.executable,
        str(SEED_HELPER),
        cwd,
        str(dst),
        str(preferred) if preferred is not None else "",
    ]
    if offset:
        args.append(str(offset))
    subprocess.run(args, check=True, env=env)
    return dst.read_text()


def test_seed_helper_returns_latest_assistant_text_when_tail_is_assistant(tmp_path):
    transcript = tmp_path / "session.jsonl"
    _write_jsonl(
        transcript,
        [
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}},
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "latest"}]}},
        ],
    )

    assert _run_seed_helper(tmp_path, cwd="/repo", preferred=transcript) == "latest\n"


def test_seed_helper_can_fall_back_across_newer_user_message(tmp_path):
    transcript = tmp_path / "session.jsonl"
    _write_jsonl(
        transcript,
        [
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "old assistant"}]}},
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "Request cancelled by user"}]}},
        ],
    )

    assert _run_seed_helper(tmp_path, cwd="/repo", preferred=transcript) == "old assistant\n"


def test_seed_helper_survives_realistic_edit_noise_transcript(tmp_path):
    transcript = tmp_path / "session.jsonl"
    _write_jsonl(
        transcript,
        [
            {
                "type": "message",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "text",
                            "text": "# 已按你选的方向改回去\n\n所以这次是按你选的“严格同构 dotfiles 原版”落地了。\n",
                        }
                    ],
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "<system-notification>\nCommand file: /Users/notdp/.factory/commands/cvim\n</system-notification>",
                        }
                    ],
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "Error: Request was aborted."}],
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "Request cancelled by user"}],
                },
            },
            {
                "type": "message",
                "message": {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "<droid_edit mode=\"diff\">\n<edit_target>previous_assistant_message</edit_target>\n<edit_note>请把下面的 diff 视为用户对紧邻上一条 assistant message 的修改。</edit_note>\n<edit_diff>\n--- before\n+++ after\n@@ -1,2 +1,4 @@\n \n 所以这次是按你选的“严格同构 dotfiles 原版”落地了。\n+\n+貌似可以了哦\n</edit_diff>\n</droid_edit>",
                        }
                    ],
                },
            },
        ],
    )

    assert _run_seed_helper(tmp_path, cwd="/repo", preferred=transcript) == (
        "# 已按你选的方向改回去\n\n所以这次是按你选的“严格同构 dotfiles 原版”落地了。\n"
    )


def test_seed_helper_scans_factory_sessions_when_preferred_missing(tmp_path):
    factory_home = tmp_path / "factory"
    transcript_dir = factory_home / "sessions" / "-repo"
    transcript_dir.mkdir(parents=True)
    transcript = transcript_dir / "abc.jsonl"
    _write_jsonl(
        transcript,
        [
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "from session dir"}]}},
        ],
    )

    assert _run_seed_helper(tmp_path, cwd="/repo", factory_home=factory_home) == "from session dir\n"


def test_seed_helper_offset_returns_earlier_assistant_message(tmp_path):
    transcript = tmp_path / "session.jsonl"
    _write_jsonl(
        transcript,
        [
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "first"}]}},
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "ok"}]}},
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "second"}]}},
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "more"}]}},
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "third"}]}},
        ],
    )

    assert _run_seed_helper(tmp_path, cwd="/repo", preferred=transcript, offset=0) == "third\n"
    assert _run_seed_helper(tmp_path, cwd="/repo", preferred=transcript, offset=1) == "second\n"
    assert _run_seed_helper(tmp_path, cwd="/repo", preferred=transcript, offset=2) == "first\n"
    assert _run_seed_helper(tmp_path, cwd="/repo", preferred=transcript, offset=3) == ""


def test_seed_helper_session_mode_resolves_transcript_before_extracting(tmp_path):
    transcript = tmp_path / "mapped.jsonl"
    _write_jsonl(
        transcript,
        [
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "from pid map"}]}},
        ],
    )
    session_map_file = tmp_path / "session-map.json"
    session_map_file.write_text(json.dumps({"by_pid": {"111": {"transcript_path": str(transcript)}}}))
    dst = tmp_path / "seed.txt"

    subprocess.run(
        [
            sys.executable,
            str(SEED_HELPER),
            "/repo",
            str(dst),
            str(session_map_file),
            "111",
            "ttys001",
            "droid --resume 12345678-1234-1234-1234-123456789abc",
        ],
        check=True,
    )

    assert dst.read_text() == "from pid map\n"
