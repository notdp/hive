"""The acceptance oracle's own transcript readers.

A node's JSON line says what the runner believed; these readers say what the
member's engine wrote. They resolve a member's transcript from its registry
row (cli, sessionId, cwd) the way the engines lay files out, find the one
input record carrying the dispatch id, and read the turn that input started
to its terminal record and final assistant text — independently of the
runner's own readers in `crates/hive/src/adapters/*_turn.rs`, so the two
can disagree.

Record shapes (verified on live transcripts, 2026-09-06):

- claude `~/.claude/projects/<cwd slug>/<sid>.jsonl`: a human-origin `user`
  record carries the envelope as `message.content` (a string, or text
  blocks); a `tool_result` row is a `user` record too, with a
  `tool_result` block. Every record has `uuid`/`parentUuid`; the assistant
  record closing the turn has `message.stop_reason == "end_turn"`, its text
  in `message.content[].text`.
- codex `~/.codex/sessions/**/rollout-*-<sid>.jsonl`: the input is a
  `response_item` whose payload is a `message` with `role: user` and an
  `input_text` block; `payload.internal_chat_message_metadata_passthrough
  .turn_id` names the turn; `event_msg` `task_complete` with the same
  `turn_id` closes it and carries `last_agent_message`.
- grok `~/.grok/sessions/<urlencode(cwd, safe='')>/<sid>/`:
  `chat_history.jsonl` records of `type: user` whose text holds
  `<user_query>` are turn inputs (system-reminder user records are not);
  `events.jsonl` `turn_started` (`session_id`, `turn_number`) and
  `turn_ended` (`outcome`) pair up in order, one pair per query.
"""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import quote

DISPATCH_ID_RE = re.compile(r"^nd-[0-9a-f]{12}$")


@dataclass
class BoundTurn:
    """What the member's transcript says about one dispatch id."""

    input_count: int = 0  # input records carrying the marker
    turn: str = ""  # engine turn key of the first such input
    terminal: bool = False  # the turn's terminal record exists
    outcome: str = ""  # "completed" or the engine's own label
    blocks: list[str] = field(default_factory=list)  # final message text blocks, in order

    @property
    def text(self) -> str:
        return "\n".join(self.blocks)


def normalize(text: str) -> str:
    """Whitespace-insensitive form for comparing a body with the transcript:
    the join between text blocks is the runner's choice, the words are not."""
    return re.sub(r"\s+", "", text)


def read_jsonl(path: Path) -> list[dict]:
    """Complete records only: a half-written last line is not a record."""
    out: list[dict] = []
    try:
        raw = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return out
    for line in raw.split("\n"):
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if isinstance(rec, dict):
            out.append(rec)
    return out


# --- locating a member's transcript ---


def claude_transcript(session_id: str, cwd: str, home: Path | None = None) -> Path | None:
    root = Path(os.environ.get("CLAUDE_CONFIG_DIR") or (home or Path.home()) / ".claude") / "projects"
    slug = re.sub(r"[^A-Za-z0-9]", "-", cwd)
    direct = root / slug / f"{session_id}.jsonl"
    if direct.exists():
        return direct
    return next(iter(sorted(root.glob(f"*/{session_id}.jsonl"))), None)


def codex_transcript(session_id: str, home: Path | None = None) -> Path | None:
    root = Path(os.environ.get("CODEX_HOME") or (home or Path.home()) / ".codex") / "sessions"
    return next(iter(sorted(root.glob(f"**/rollout-*-{session_id}.jsonl"))), None)


def grok_session_dir(session_id: str, cwd: str, home: Path | None = None) -> Path | None:
    root = Path(os.environ.get("GROK_HOME") or (home or Path.home()) / ".grok") / "sessions"
    direct = root / quote(cwd, safe="") / session_id
    if direct.is_dir():
        return direct
    return next(iter(sorted(p for p in root.glob(f"*/{session_id}") if p.is_dir())), None)


def read_member_turn(cli: str, session_id: str, cwd: str, marker: str, home: Path | None = None) -> BoundTurn:
    """Locate the member's transcript and read the turn `marker` started.
    A missing transcript reads as an empty BoundTurn (no input found)."""
    if cli == "claude":
        path = claude_transcript(session_id, cwd, home)
        return claude_turn(read_jsonl(path), marker) if path else BoundTurn()
    if cli == "codex":
        path = codex_transcript(session_id, home)
        return codex_turn(read_jsonl(path), marker) if path else BoundTurn()
    if cli == "grok":
        d = grok_session_dir(session_id, cwd, home)
        if not d:
            return BoundTurn()
        return grok_turn(read_jsonl(d / "chat_history.jsonl"), read_jsonl(d / "events.jsonl"), marker)
    raise ValueError(f"no transcript reader for cli {cli!r}")


# --- claude ---


def _claude_input_text(rec: dict) -> str | None:
    """The text of a human-origin user record; None for anything else
    (tool results, meta/companion rows, assistant records)."""
    if rec.get("type") != "user" or rec.get("isMeta") or rec.get("turnCompanion"):
        return None
    content = (rec.get("message") or {}).get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list) and content and all(
        isinstance(b, dict) and b.get("type") == "text" for b in content
    ):
        return "\n".join(str(b.get("text", "")) for b in content)
    return None


def claude_turn(records: list[dict], marker: str) -> BoundTurn:
    bound = BoundTurn()
    inputs = [i for i, r in enumerate(records) if marker in (_claude_input_text(r) or "")]
    bound.input_count = len(inputs)
    if not inputs:
        return bound
    start = inputs[0]
    anchor = records[start]
    bound.turn = str(anchor.get("uuid", ""))
    chain = {bound.turn}
    terminal: dict | None = None
    for rec in records[start + 1:]:
        if rec.get("parentUuid") not in chain:
            continue
        if _claude_input_text(rec) is not None:
            break  # a fresh human input chained in: the turn ended without end_turn
        chain.add(str(rec.get("uuid", "")))
        msg = rec.get("message") or {}
        if rec.get("type") == "assistant" and msg.get("stop_reason") == "end_turn":
            terminal = rec
            break
    if terminal is None:
        return bound
    bound.terminal = True
    bound.outcome = "completed"
    # claude writes one record per API block: the final message is every
    # assistant record on the chain sharing the terminal record's message id,
    # including the blocks written after the one carrying end_turn.
    message_id = (terminal.get("message") or {}).get("id")
    for rec in records[start + 1:]:
        msg = rec.get("message") or {}
        if rec.get("type") != "assistant" or msg.get("id") != message_id:
            continue
        if rec.get("uuid") in chain or rec.get("parentUuid") in chain:
            chain.add(str(rec.get("uuid", "")))
        else:
            continue
        for block in msg.get("content") or []:
            if isinstance(block, dict) and block.get("type") == "text":
                bound.blocks.append(str(block.get("text", "")))
    return bound


# --- codex ---


def _codex_user_text(rec: dict) -> str | None:
    if rec.get("type") != "response_item":
        return None
    payload = rec.get("payload") or {}
    if payload.get("type") != "message" or payload.get("role") != "user":
        return None
    texts = [
        str(b.get("text", ""))
        for b in payload.get("content") or []
        if isinstance(b, dict) and b.get("type") == "input_text"
    ]
    return "\n".join(texts) if texts else None


def _codex_turn_id(rec: dict) -> str:
    payload = rec.get("payload") or {}
    meta = payload.get("internal_chat_message_metadata_passthrough") or {}
    return str(meta.get("turn_id") or payload.get("turn_id") or "")


def codex_turn(records: list[dict], marker: str) -> BoundTurn:
    bound = BoundTurn()
    inputs = [r for r in records if marker in (_codex_user_text(r) or "")]
    bound.input_count = len(inputs)
    if not inputs:
        return bound
    bound.turn = _codex_turn_id(inputs[0])
    if not bound.turn:
        return bound
    final_answer: list[str] = []
    for rec in records:
        payload = rec.get("payload") or {}
        if _codex_turn_id(rec) != bound.turn:
            continue
        if rec.get("type") == "response_item" and payload.get("role") == "assistant" and payload.get("phase") == "final_answer":
            final_answer = [
                str(b.get("text", ""))
                for b in payload.get("content") or []
                if isinstance(b, dict) and b.get("type") == "output_text"
            ]
        if rec.get("type") == "event_msg" and payload.get("type") == "task_complete":
            bound.terminal = True
            bound.outcome = "completed"
            last = payload.get("last_agent_message")
            bound.blocks = [str(last)] if last is not None else final_answer
            return bound
        if rec.get("type") == "event_msg" and payload.get("type") in ("turn_aborted", "error"):
            bound.terminal = True
            bound.outcome = str(payload.get("type"))
            return bound
    return bound


# --- grok ---


def _grok_text(rec: dict) -> str:
    content = rec.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(
            str(b.get("text", "")) for b in content if isinstance(b, dict) and b.get("type") == "text"
        )
    return ""


def _grok_is_query(rec: dict) -> bool:
    return rec.get("type") == "user" and "<user_query>" in _grok_text(rec)


def grok_turn(history: list[dict], events: list[dict], marker: str) -> BoundTurn:
    bound = BoundTurn()
    queries = [i for i, r in enumerate(history) if _grok_is_query(r)]
    hits = [i for i in queries if marker in _grok_text(history[i])]
    bound.input_count = len(hits)
    if not hits:
        return bound
    start = hits[0]
    ordinal = queries.index(start)
    starts = [e for e in events if e.get("type") == "turn_started"]
    ends = [e for e in events if e.get("type") == "turn_ended"]
    if ordinal < len(starts):
        s = starts[ordinal]
        bound.turn = f"{s.get('session_id', '')}/{s.get('turn_number', '')}"
    if ordinal < len(ends):
        bound.terminal = True
        bound.outcome = str(ends[ordinal].get("outcome", ""))
    stop = queries[ordinal + 1] if ordinal + 1 < len(queries) else len(history)
    last_assistant = None
    for rec in history[start + 1:stop]:
        if rec.get("type") == "assistant":
            last_assistant = rec
    if last_assistant is not None and bound.terminal:
        text = _grok_text(last_assistant)
        bound.blocks = [text] if text else []
    return bound
