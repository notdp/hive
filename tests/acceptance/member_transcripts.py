"""The acceptance oracle's own transcript readers.

A node's JSON line says what the runner believed; these readers say what the
member's engine wrote. They resolve a member's engine session from its
registry row (cli, sessionId, cwd), locate the transcript the way the
engines lay files out, find the one input record carrying the dispatch id,
and read the turn that input started to its terminal record and final
assistant text — independently of the runner's own readers in
`crates/hive/src/adapters/*_turn.rs`, so the two can disagree. Nothing here
takes the node's own answer (its `session`, `turn` or `body`) as a key to
find anything.

Identity: a codex or grok registry row holds the engine's own session id.
A claude row holds the bg job id (8 hex, the leading block of the session
uuid), and the engine session behind it is read from the job's own state
file `<claude-config>/jobs/<job_id>/state.json` (`sessionId`), which is not
the sessions-registry entry the runner resolves it through.

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
  `chat_history.jsonl` records of `type: user` with a `prompt_index` are
  the prompts (a mid-turn user record carries `synthetic_reason` and no
  `prompt_index`); an assistant record with `tool_calls` is a step, the
  answer is an assistant record without. `events.jsonl` `turn_started`
  carries `session_id` and `turn_number` — the same coordinate as the
  prompt's `prompt_index` — and `turn_ended` (`outcome`) carries no turn
  number: start and end pair by walking the stream in order.
"""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import quote

DISPATCH_ID_RE = re.compile(r"^nd-[0-9a-f]{12}$")
# A claude bg job id: the session uuid's leading 8 hex, with the same band
# the runner accepts (`claude_bg::looks_like_job_id`).
JOB_ID_RE = re.compile(r"^[0-9a-f]{6,12}$")


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


# --- the engine session behind a registry row ---


def claude_config_dir(home: Path | None = None) -> Path:
    return Path(os.environ.get("CLAUDE_CONFIG_DIR") or (home or Path.home()) / ".claude")


def claude_job_session(job_id: str, home: Path | None = None) -> str:
    """The engine session a claude bg job runs, from the job's own state
    file `<claude-config>/jobs/<job_id>/state.json` (`sessionId`). Empty
    when the job has no readable state."""
    try:
        data = json.loads((claude_config_dir(home) / "jobs" / job_id / "state.json").read_text())
    except (OSError, ValueError):
        return ""
    sid = data.get("sessionId") if isinstance(data, dict) else None
    return sid if isinstance(sid, str) else ""


def engine_session(cli: str, roster_session: str, home: Path | None = None) -> str:
    """The engine session id a registry row's `sessionId` stands for: a
    claude row holds the bg job id and resolves through the job's state
    file; any other value is the engine id itself."""
    if cli == "claude" and JOB_ID_RE.match(roster_session):
        return claude_job_session(roster_session, home)
    return roster_session


# --- locating a member's transcript ---


def claude_transcript(session_id: str, cwd: str, home: Path | None = None) -> Path | None:
    root = claude_config_dir(home) / "projects"
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
    """Locate the member's transcript by its engine session id (see
    `engine_session`) and read the turn `marker` started. A missing
    transcript reads as an empty BoundTurn (no input found)."""
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


def _grok_prompt_index(rec: dict) -> int | None:
    """The turn number of a prompt record; None for a mid-turn user record
    (`synthetic_reason`, no `prompt_index`) and for anything else."""
    if rec.get("type") != "user":
        return None
    index = rec.get("prompt_index")
    return index if isinstance(index, int) and not isinstance(index, bool) else None


def grok_turn(history: list[dict], events: list[dict], marker: str) -> BoundTurn:
    bound = BoundTurn()
    hits = [i for i, r in enumerate(history) if _grok_prompt_index(r) is not None and marker in _grok_text(r)]
    bound.input_count = len(hits)
    if not hits:
        return bound
    start = hits[0]
    number = _grok_prompt_index(history[start])
    # The prompt's `prompt_index` is the turn number. `turn_ended` carries
    # none, so the pairing walks the event stream in order from the
    # `turn_started` with that number: the first `turn_ended` after it closes
    # the turn; another `turn_started` first means the turn never got its own
    # end record. Splitting the stream into two arrays and indexing by
    # ordinal would hand a turn without an end the next turn's outcome.
    opened = False
    for event in events:
        kind = event.get("type")
        if not opened:
            if kind == "turn_started" and event.get("turn_number") == number:
                opened = True
                bound.turn = f"{event.get('session_id', '')}/{number}"
            continue
        if kind == "turn_started":
            break
        if kind == "turn_ended":
            bound.terminal = True
            bound.outcome = str(event.get("outcome", ""))
            break
    # The final message is the last assistant record of the turn's history
    # span (up to the next prompt) that carries no `tool_calls`: a
    # tool-calling record is a step, and its narration is not the answer.
    final = None
    for rec in history[start + 1:]:
        if _grok_prompt_index(rec) is not None:
            break
        if rec.get("type") == "assistant" and not rec.get("tool_calls"):
            final = rec
    if final is not None and bound.terminal:
        text = _grok_text(final)
        bound.blocks = [text] if text else []
    return bound
