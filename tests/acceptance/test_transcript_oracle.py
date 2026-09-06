"""Dry run of the acceptance oracle's transcript readers against generated
fixtures shaped like the three engines' real records. Not gated behind
HIVE_ACCEPTANCE: nothing here spawns anything or reads a real home.

Each fixture plants the traps the live oracle exists for: the dispatch id
and nonce in the input record (a reader that returns the input would "find"
the nonce), a tool_result row quoting the task path, a later turn whose
assistant text carries a decoy, a half-written trailing line, and — for
claude — a registry row holding the bg job id rather than the session uuid
the transcript is named after.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from member_transcripts import (
    DISPATCH_ID_RE,
    JOB_ID_RE,
    claude_turn,
    codex_turn,
    engine_session,
    grok_turn,
    normalize,
    read_jsonl,
    read_member_turn,
)

DID = "nd-0123456789ab"
NONCE = "acc-777777-x"
BAIT = "bait-777777-x"
ENVELOPE = f"<HIVE to=acc.probe artifact=/ws/artifacts/tasks/probe-{DID}.md>\ntask {DID}\n请写 {NONCE},别复述 {BAIT}\n</HIVE>"
FINAL_A = f"口令是 {NONCE}。"
FINAL_B = "任务很简单。"


def jsonl(records: list[dict], half_line: str = "") -> str:
    return "".join(json.dumps(r, ensure_ascii=False) + "\n" for r in records) + half_line


def claude_records(sid: str = "sid-c") -> list[dict]:
    def rec(kind, uuid, parent, message, **extra):
        r = {"parentUuid": parent, "type": kind, "uuid": uuid, "sessionId": sid, "message": message}
        r.update(extra)
        return r

    return [
        rec("user", "u0", None, {"role": "user", "content": "/hive acc"}),
        rec("assistant", "a0", "u0", {"id": "m0", "role": "assistant", "stop_reason": "end_turn",
                                     "content": [{"type": "text", "text": "在队里了。"}]}),
        rec("user", "u1", "a0", {"role": "user", "content": ENVELOPE}, origin={"kind": "human"}),
        rec("assistant", "a1", "u1", {"id": "m1", "role": "assistant", "stop_reason": "tool_use",
                                     "content": [{"type": "thinking", "thinking": "..."}]}),
        rec("assistant", "a2", "a1", {"id": "m1", "role": "assistant", "stop_reason": "tool_use",
                                     "content": [{"type": "tool_use", "id": "t1", "name": "Read",
                                                  "input": {"file_path": f"/ws/artifacts/tasks/probe-{DID}.md"}}]}),
        rec("user", "u2", "a2", {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                                                            "content": f"task {DID} {NONCE}"}]},
            toolUseResult={"file": f"/ws/artifacts/tasks/probe-{DID}.md"}),
        rec("assistant", "a3", "u2", {"id": "m2", "role": "assistant", "stop_reason": "end_turn",
                                     "content": [{"type": "text", "text": FINAL_A}]}),
        rec("assistant", "a4", "a3", {"id": "m2", "role": "assistant", "stop_reason": "end_turn",
                                     "content": [{"type": "text", "text": FINAL_B}]}),
        rec("user", "u3", "a4", {"role": "user", "content": "再说一遍干扰词"}, origin={"kind": "human"}),
        rec("assistant", "a5", "u3", {"id": "m3", "role": "assistant", "stop_reason": "end_turn",
                                     "content": [{"type": "text", "text": BAIT}]}),
    ]


def test_claude_reader_binds_the_input_and_reads_the_final_message():
    turn = claude_turn(claude_records(), DID)
    assert turn.input_count == 1  # the tool_result row quoting the path is not an input
    assert turn.turn == "u1"
    assert turn.terminal and turn.outcome == "completed"
    assert turn.blocks == [FINAL_A, FINAL_B]  # both blocks of the final message, in order
    assert BAIT not in turn.text and NONCE in turn.text


def test_claude_reader_reports_a_turn_without_end_turn_as_open():
    records = claude_records()
    del records[6:8]  # the closing message never landed; the next human input did
    turn = claude_turn(records, DID)
    assert turn.turn == "u1" and not turn.terminal and turn.blocks == []


def test_claude_reader_skips_meta_rows_and_half_lines(tmp_path):
    records = claude_records()
    meta = dict(records[2], uuid="meta", isMeta=True, turnCompanion=True)
    records.insert(2, meta)
    path = tmp_path / "t.jsonl"
    path.write_text(jsonl(records, half_line='{"type":"assistant","uuid":"a9","parentUuid":"a5","mess'))
    parsed = read_jsonl(path)
    assert len(parsed) == len(records)
    turn = claude_turn(parsed, DID)
    assert turn.input_count == 1 and turn.turn == "u1"


def codex_records(turn_id: str = "turn-1") -> list[dict]:
    def item(payload, ordinal):
        return {"timestamp": "2026-09-06T10:00:00.000Z", "ordinal": ordinal, "type": "response_item", "payload": payload}

    def event(payload, ordinal):
        return {"timestamp": "2026-09-06T10:00:00.000Z", "ordinal": ordinal, "type": "event_msg", "payload": payload}

    meta = {"internal_chat_message_metadata_passthrough": {"turn_id": turn_id}}
    meta2 = {"internal_chat_message_metadata_passthrough": {"turn_id": "turn-2"}}
    return [
        event({"type": "task_started", "turn_id": turn_id}, 1),
        item({"type": "message", "role": "user", "content": [{"type": "input_text", "text": ENVELOPE}], **meta}, 2),
        item({"type": "custom_tool_call", "name": "shell", "input": f"cat /ws/artifacts/tasks/probe-{DID}.md", **meta}, 3),
        item({"type": "custom_tool_call_output", "output": f"task {DID} {NONCE}", **meta}, 4),
        item({"type": "message", "role": "assistant", "phase": "commentary",
              "content": [{"type": "output_text", "text": "读完了。"}], **meta}, 5),
        item({"type": "message", "role": "assistant", "phase": "final_answer",
              "content": [{"type": "output_text", "text": FINAL_A + "\n" + FINAL_B}], **meta}, 6),
        event({"type": "task_complete", "turn_id": turn_id, "last_agent_message": FINAL_A + "\n" + FINAL_B}, 7),
        event({"type": "task_started", "turn_id": "turn-2"}, 8),
        item({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "再说一遍干扰词"}], **meta2}, 9),
        event({"type": "task_complete", "turn_id": "turn-2", "last_agent_message": BAIT}, 10),
    ]


def test_codex_reader_binds_turn_id_and_reads_task_complete():
    turn = codex_turn(codex_records(), DID)
    assert turn.input_count == 1 and turn.turn == "turn-1"
    assert turn.terminal and turn.outcome == "completed"
    assert normalize(turn.text) == normalize(FINAL_A + FINAL_B)
    assert BAIT not in turn.text


def test_codex_reader_does_not_take_another_turns_completion():
    records = [r for r in codex_records() if not (
        r["type"] == "event_msg" and r["payload"].get("turn_id") == "turn-1" and r["payload"]["type"] == "task_complete"
    )]
    turn = codex_turn(records, DID)
    assert turn.turn == "turn-1" and not turn.terminal and turn.blocks == []


def grok_files(outcome: str = "completed") -> tuple[list[dict], list[dict]]:
    def prompt(index: int, text: str) -> dict:
        return {"type": "user", "content": [{"type": "text", "text": f"<user_query>\n{text}\n</user_query>"}],
                "prompt_index": index}

    def reminder(text: str) -> dict:
        return {"type": "user", "content": [{"type": "text", "text": f"<system-reminder>\n{text}\n</system-reminder>"}],
                "synthetic_reason": "system_reminder"}

    history = [
        {"type": "system", "content": "You are Grok."},
        reminder("skills…"),
        prompt(0, "/hive acc"),
        {"type": "assistant", "content": "在队里了。", "model_id": "grok-4.6"},
        reminder(f"mcp {DID} mentions do not count"),
        prompt(1, ENVELOPE),
        {"type": "reasoning", "content": None},
        # a tool-calling step narrates: its text is not the answer
        {"type": "assistant", "content": "先读任务文件。", "model_id": "grok-4.6",
         "tool_calls": [{"id": "call-1", "name": "read_file", "arguments": "{}"}]},
        {"type": "tool_result", "content": f"task {DID} {NONCE}", "tool_call_id": "call-1"},
        {"type": "assistant", "content": [{"type": "text", "text": FINAL_A}, {"type": "text", "text": FINAL_B}],
         "model_id": "grok-4.6"},
        prompt(2, "再说一遍干扰词"),
        {"type": "assistant", "content": BAIT, "model_id": "grok-4.6"},
    ]
    events = [
        {"type": "turn_started", "session_id": "sid-g", "turn_number": 0},
        {"type": "phase_changed", "phase": "waiting_for_model"},
        {"type": "turn_ended", "outcome": "completed"},
        {"type": "turn_started", "session_id": "sid-g", "turn_number": 1},
        {"type": "tool_started", "name": "read_file"},
        {"type": "turn_ended", "outcome": outcome},
        {"type": "turn_started", "session_id": "sid-g", "turn_number": 2},
        {"type": "turn_ended", "outcome": "completed"},
    ]
    return history, events


def test_grok_reader_binds_the_prompt_index_to_its_turn_events():
    turn = grok_turn(*grok_files(), DID)
    assert turn.input_count == 1  # the system-reminder user record carrying the id is not a prompt
    assert turn.turn == "sid-g/1"
    assert turn.terminal and turn.outcome == "completed"
    assert normalize(turn.text) == normalize(FINAL_A + FINAL_B)  # not the tool step's narration
    assert BAIT not in turn.text


def test_grok_reader_keeps_a_non_completed_outcome_label():
    turn = grok_turn(*grok_files(outcome="cancelled"), DID)
    assert turn.terminal and turn.outcome == "cancelled"


def test_grok_reader_pairs_events_in_order_not_by_ordinal():
    # turn 1 never got its own turn_ended; turn 2 started and completed.
    # Splitting the stream into a starts array and an ends array and
    # indexing both by the prompt's ordinal would give turn 1 turn 2's end.
    history, events = grok_files()
    del events[5]  # turn 1's turn_ended
    turn = grok_turn(history, events, DID)
    assert turn.turn == "sid-g/1"
    assert not turn.terminal and turn.outcome == "" and turn.blocks == []


def test_grok_reader_final_message_waits_for_the_non_tool_record():
    # turn_ended lands before the answer's history line: with only the
    # tool-calling record on disk the turn is terminal but has no text yet
    history, events = grok_files()
    del history[9]  # the final assistant record
    turn = grok_turn(history, events, DID)
    assert turn.terminal and turn.outcome == "completed" and turn.blocks == []


def test_read_member_turn_resolves_each_engines_layout(tmp_path, monkeypatch):
    for var in ("CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME"):
        monkeypatch.delenv(var, raising=False)
    cwd = "/Users/x/Developer/hive"
    home = tmp_path
    c = home / ".claude" / "projects" / "-Users-x-Developer-hive"
    c.mkdir(parents=True)
    (c / "sid-c.jsonl").write_text(jsonl(claude_records()))
    k = home / ".codex" / "sessions" / "2026" / "09" / "06"
    k.mkdir(parents=True)
    (k / "rollout-2026-09-06T10-00-00-sid-k.jsonl").write_text(jsonl(codex_records()))
    g = home / ".grok" / "sessions" / "%2FUsers%2Fx%2FDeveloper%2Fhive" / "sid-g"
    g.mkdir(parents=True)
    history, events = grok_files()
    (g / "chat_history.jsonl").write_text(jsonl(history))
    (g / "events.jsonl").write_text(jsonl(events))

    for cli, sid, key in (("claude", "sid-c", "u1"), ("codex", "sid-k", "turn-1"), ("grok", "sid-g", "sid-g/1")):
        turn = read_member_turn(cli, sid, cwd, DID, home=home)
        assert turn.turn == key, (cli, turn)
        assert turn.terminal and NONCE in turn.text and BAIT not in turn.text, (cli, turn)
    assert read_member_turn("claude", "missing", cwd, DID, home=home).input_count == 0
    with pytest.raises(ValueError):
        read_member_turn("stub", "sid", cwd, DID, home=home)


def test_claude_job_id_roster_row_resolves_through_the_jobs_state_file(tmp_path, monkeypatch):
    # The registry row of a claude bg member holds the job id (8 hex, the
    # session uuid's leading block); the transcript is named after the
    # uuid. The oracle resolves job -> session from the job's own state
    # file and never from the node's answer.
    monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
    home = tmp_path
    job_id = "b50e8587"
    session = "b50e8587-aec4-4b85-8bdd-db6d040d75eb"
    cwd = "/Users/x/Developer/hive"
    assert JOB_ID_RE.match(job_id) and not JOB_ID_RE.match(session)
    job = home / ".claude" / "jobs" / job_id
    job.mkdir(parents=True)
    (job / "state.json").write_text(json.dumps({
        "name": "acc.probe-claude", "state": "running", "cwd": cwd,
        "sessionId": session, "resumeSessionId": None, "backend": "local",
    }))
    projects = home / ".claude" / "projects" / "-Users-x-Developer-hive"
    projects.mkdir(parents=True)
    (projects / f"{session}.jsonl").write_text(jsonl(claude_records(sid=session)))

    assert engine_session("claude", job_id, home=home) == session
    assert engine_session("claude", session, home=home) == session  # a uuid row passes through
    assert engine_session("claude", "deadbeef", home=home) == ""  # a job without state resolves to nothing
    assert engine_session("codex", "abcdef12", home=home) == "abcdef12"  # only claude rows are job ids
    assert engine_session("grok", "abcdef12", home=home) == "abcdef12"

    turn = read_member_turn("claude", engine_session("claude", job_id, home=home), cwd, DID, home=home)
    assert turn.turn == "u1" and turn.terminal and NONCE in turn.text and BAIT not in turn.text
    # the job id names no transcript: reading by it finds nothing
    assert read_member_turn("claude", job_id, cwd, DID, home=home).input_count == 0


def test_dispatch_id_shape():
    assert DISPATCH_ID_RE.match(DID)
    for bad in ("nd-0123456789AB", "nd-0123456789a", "flow.run", ""):
        assert not DISPATCH_ID_RE.match(bad), bad
