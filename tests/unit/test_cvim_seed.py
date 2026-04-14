import json
import os
import subprocess
import sys
from types import SimpleNamespace
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SEED_HELPER = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin" / "cvim-seed"
SHARED_DIR = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin"


def _import_shared():
    if str(SHARED_DIR) not in sys.path:
        sys.path.insert(0, str(SHARED_DIR))
    import _cvim_shared

    return _cvim_shared


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


def test_extract_last_assistant_text_reads_claude_transcript(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "claude.jsonl"
    _write_jsonl(
        transcript,
        [
            {"type": "permission-mode", "permissionMode": "bypassPermissions"},
            {
                "type": "user",
                "sessionId": "sess-claude",
                "cwd": "/repo",
                "uuid": "u1",
                "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            },
            {
                "type": "assistant",
                "sessionId": "sess-claude",
                "cwd": "/repo",
                "uuid": "u2",
                "parentUuid": "u1",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "from claude"}],
                },
            },
        ],
    )

    assert shared.extract_last_assistant_text(transcript) == "from claude"


def test_extract_last_assistant_text_reads_codex_transcript(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "codex.jsonl"
    _write_jsonl(
        transcript,
        [
            {"type": "session_meta", "payload": {"id": "sess-codex", "cwd": "/repo"}},
            {
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "from codex"}],
                },
            },
        ],
    )

    assert shared.extract_last_assistant_text(transcript) == "from codex"


def test_extract_last_assistant_text_skips_codex_skill_turn_commentary(tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "codex-skill.jsonl"
    _write_jsonl(
        transcript,
        [
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
        ],
    )

    assert shared.resolve_assistant_offset(transcript) == 1
    assert shared.extract_last_assistant_text(transcript) == "真正要编辑的回答"


def test_resolve_transcript_path_for_non_droid_pane_uses_adapter(monkeypatch, tmp_path):
    shared = _import_shared()
    transcript = tmp_path / "claude.jsonl"
    transcript.write_text("")

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            assert pane_id == "%42"
            return "sess-claude"

        def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
            assert session_id == "sess-claude"
            assert cwd == "/repo"
            return transcript

    monkeypatch.setattr(shared, "_detect_profile_for_pane", lambda pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr(shared, "_get_adapter", lambda name: FakeAdapter() if name == "claude" else None)

    assert shared.resolve_transcript_path_for_pane(
        pane_id="%42",
        cwd="/repo",
    ) == str(transcript)


def test_resolve_transcript_path_for_non_droid_pane_returns_none_without_resume(monkeypatch, tmp_path):
    shared = _import_shared()

    class MissingAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            return None

        def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
            raise AssertionError("find_session_file should not be called when session id is missing")

    monkeypatch.setattr(shared, "_detect_profile_for_pane", lambda pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr(shared, "_get_adapter", lambda name: MissingAdapter() if name == "claude" else None)

    assert shared.resolve_transcript_path_for_pane(
        pane_id="%42",
        cwd="/repo",
    ) is None


def test_resolve_transcript_path_for_pane_falls_back_to_resume_transcript(monkeypatch, tmp_path):
    shared = _import_shared()
    factory_home = tmp_path / "factory"
    monkeypatch.setenv("FACTORY_HOME", str(factory_home))
    resumed = factory_home / "sessions" / "-repo" / "12345678-1234-1234-1234-123456789abc.jsonl"
    resumed.parent.mkdir(parents=True)
    resumed.write_text("")

    class PartialAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            assert pane_id == "%42"
            return "sess-claude"

        def find_session_file(self, session_id: str, *, cwd: str | None = None) -> Path | None:
            assert session_id == "sess-claude"
            assert cwd == "/repo"
            return tmp_path / "missing.jsonl"

    monkeypatch.setattr(shared, "_detect_profile_for_pane", lambda pane_id: SimpleNamespace(name="claude"))
    monkeypatch.setattr(shared, "_get_adapter", lambda name: PartialAdapter() if name == "claude" else None)

    assert shared.resolve_transcript_path_for_pane(
        pane_id="%42",
        cwd="/repo",
        droid_args="droid --resume 12345678-1234-1234-1234-123456789abc",
    ) == str(resumed)
