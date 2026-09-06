"""Dry run of the acceptance oracle's transcript readers against generated
fixtures shaped like the three engines' real records. Not gated behind
HIVE_ACCEPTANCE: nothing here spawns anything or reads a real home.

The readers count how many input records carry the dispatch id, and each
fixture plants the rows that must not count: a tool_result row quoting the
task path, a harness companion row, a grok system-reminder carrying the id,
a half-written trailing line — and, for claude, a registry row holding the
bg job id rather than the session uuid the transcript is named after.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from member_transcripts import (
    DISPATCH_ID_RE,
    JOB_ID_RE,
    claude_inputs,
    codex_inputs,
    count_dispatch_inputs,
    engine_session,
    grok_inputs,
    read_jsonl,
)

DID = "nd-0123456789ab"
NONCE = "acc-777777-x"
ENVELOPE = f"<HIVE to=acc.probe artifact=/ws/artifacts/tasks/probe-{DID}.md>\ntask {DID}\n请写 {NONCE}\n</HIVE>"


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
                                     "content": [{"type": "tool_use", "id": "t1", "name": "Read",
                                                  "input": {"file_path": f"/ws/artifacts/tasks/probe-{DID}.md"}}]}),
        rec("user", "u2", "a1", {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                                                            "content": f"task {DID} {NONCE}"}]},
            toolUseResult={"file": f"/ws/artifacts/tasks/probe-{DID}.md"}),
        rec("assistant", "a2", "u2", {"id": "m2", "role": "assistant", "stop_reason": "end_turn",
                                     "content": [{"type": "text", "text": f"returned {DID}"}]}),
    ]


def test_claude_reader_counts_the_envelope_once():
    # the tool_result row quoting the path and the assistant echo are not inputs
    assert claude_inputs(claude_records(), DID) == 1
    assert claude_inputs(claude_records(), "nd-ffffffffffff") == 0


def test_claude_reader_skips_meta_rows_and_half_lines(tmp_path):
    records = claude_records()
    records.insert(2, dict(records[2], uuid="meta", isMeta=True, turnCompanion=True))
    path = tmp_path / "t.jsonl"
    path.write_text(jsonl(records, half_line='{"type":"user","uuid":"u9","message":{"content":"' + DID))
    parsed = read_jsonl(path)
    assert len(parsed) == len(records)
    assert claude_inputs(parsed, DID) == 1


def test_claude_reader_counts_a_redelivered_envelope_twice():
    records = claude_records()
    records.append(dict(records[2], uuid="u1b", parentUuid="a2"))
    assert claude_inputs(records, DID) == 2


def codex_records(turn_id: str = "turn-1") -> list[dict]:
    def item(payload, ordinal):
        return {"timestamp": "2026-09-06T10:00:00.000Z", "ordinal": ordinal, "type": "response_item", "payload": payload}

    def event(payload, ordinal):
        return {"timestamp": "2026-09-06T10:00:00.000Z", "ordinal": ordinal, "type": "event_msg", "payload": payload}

    meta = {"internal_chat_message_metadata_passthrough": {"turn_id": turn_id}}
    return [
        event({"type": "task_started", "turn_id": turn_id}, 1),
        item({"type": "message", "role": "user", "content": [{"type": "input_text", "text": ENVELOPE}], **meta}, 2),
        item({"type": "custom_tool_call", "name": "shell", "input": f"cat /ws/artifacts/tasks/probe-{DID}.md", **meta}, 3),
        item({"type": "custom_tool_call_output", "output": f"task {DID} {NONCE}", **meta}, 4),
        item({"type": "message", "role": "assistant", "phase": "final_answer",
              "content": [{"type": "output_text", "text": f"returned {DID}"}], **meta}, 5),
        event({"type": "task_complete", "turn_id": turn_id, "last_agent_message": f"returned {DID}"}, 6),
    ]


def test_codex_reader_counts_the_user_message_once():
    # the shell call, its output and the assistant echo are not inputs
    assert codex_inputs(codex_records(), DID) == 1
    assert codex_inputs(codex_records(), "nd-ffffffffffff") == 0


def grok_history() -> list[dict]:
    def prompt(index: int, text: str) -> dict:
        return {"type": "user", "content": [{"type": "text", "text": f"<user_query>\n{text}\n</user_query>"}],
                "prompt_index": index}

    def reminder(text: str) -> dict:
        return {"type": "user", "content": [{"type": "text", "text": f"<system-reminder>\n{text}\n</system-reminder>"}],
                "synthetic_reason": "system_reminder"}

    return [
        {"type": "system", "content": "You are Grok."},
        reminder("skills…"),
        prompt(0, "/hive acc"),
        {"type": "assistant", "content": "在队里了。", "model_id": "grok-4.6"},
        reminder(f"mcp {DID} mentions do not count"),
        prompt(1, ENVELOPE),
        {"type": "assistant", "content": "先读任务文件。", "model_id": "grok-4.6",
         "tool_calls": [{"id": "call-1", "name": "read_file", "arguments": "{}"}]},
        {"type": "tool_result", "content": f"task {DID} {NONCE}", "tool_call_id": "call-1"},
        {"type": "assistant", "content": [{"type": "text", "text": f"returned {DID}"}], "model_id": "grok-4.6"},
    ]


def test_grok_reader_counts_prompts_not_reminders():
    # the system-reminder user record carrying the id is not a prompt
    assert grok_inputs(grok_history(), DID) == 1
    assert grok_inputs(grok_history(), "nd-ffffffffffff") == 0


def test_count_dispatch_inputs_resolves_each_engines_layout(tmp_path, monkeypatch):
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
    (g / "chat_history.jsonl").write_text(jsonl(grok_history()))

    for cli, sid in (("claude", "sid-c"), ("codex", "sid-k"), ("grok", "sid-g")):
        assert count_dispatch_inputs(cli, sid, cwd, DID, home=home) == 1, cli
    assert count_dispatch_inputs("claude", "missing", cwd, DID, home=home) == 0
    with pytest.raises(ValueError):
        count_dispatch_inputs("stub", "sid", cwd, DID, home=home)


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

    assert count_dispatch_inputs("claude", engine_session("claude", job_id, home=home), cwd, DID, home=home) == 1
    # the job id names no transcript: reading by it finds nothing
    assert count_dispatch_inputs("claude", job_id, cwd, DID, home=home) == 0


def test_dispatch_id_shape():
    assert DISPATCH_ID_RE.match(DID)
    for bad in ("nd-0123456789AB", "nd-0123456789a", "flow.run", ""):
        assert not DISPATCH_ID_RE.match(bad), bad
