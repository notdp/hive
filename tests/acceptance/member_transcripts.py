"""The acceptance oracle's one transcript read: how many input records of
the member's own transcript carry the dispatch id.

A node's result is the engine's own end of the turn, read by the hived
that started it, so the transcript is not where the result comes from. What it still
proves is delivery causality: the dispatch envelope landed in the member's
conversation exactly once — not zero times (the hived answered ok without
injecting), not twice (a retry after a lost answer). These readers resolve
a member's engine session from its registry row (cli, sessionId, cwd),
locate the transcript the way the engines lay files out, and count the
input records carrying the marker. Nothing here takes the node's own
answer as a key to find anything.

Identity: a codex or grok registry row holds the engine's own session id.
A claude row holds the bg job id (8 hex, the leading block of the session
uuid), and the engine session behind it is read from the job's own state
file `<claude-config>/jobs/<job_id>/state.json` (`sessionId`).

Record shapes (verified on live transcripts, 2026-09-06):

- claude `~/.claude/projects/<cwd slug>/<sid>.jsonl`: a human-origin `user`
  record carries the envelope as `message.content` (a string, or text
  blocks); a `tool_result` row is a `user` record too, with a
  `tool_result` block, and a row flagged `isMeta` or `turnCompanion` is a
  harness companion — neither is an input.
- codex `~/.codex/sessions/**/rollout-*-<sid>.jsonl`: the input is a
  `response_item` whose payload is a `message` with `role: user` and an
  `input_text` block.
- grok `~/.grok/sessions/<urlencode(cwd, safe='')>/<sid>/chat_history.jsonl`:
  records of `type: user` with a `prompt_index` are the prompts; a mid-turn
  user record (a `<system-reminder>`) carries `synthetic_reason` and no
  `prompt_index`.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from urllib.parse import quote

DISPATCH_ID_RE = re.compile(r"^nd-[0-9a-f]{12}$")
# A claude bg job id: the session uuid's leading 8 hex, with the same band
# the runner accepts (`claude_bg::looks_like_job_id`).
JOB_ID_RE = re.compile(r"^[0-9a-f]{6,12}$")


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


def count_dispatch_inputs(cli: str, session_id: str, cwd: str, marker: str, home: Path | None = None) -> int:
    """Locate the member's transcript by its engine session id (see
    `engine_session`) and count the input records carrying `marker`. A
    missing transcript counts as no input found."""
    if cli == "claude":
        path = claude_transcript(session_id, cwd, home)
        return claude_inputs(read_jsonl(path), marker) if path else 0
    if cli == "codex":
        path = codex_transcript(session_id, home)
        return codex_inputs(read_jsonl(path), marker) if path else 0
    if cli == "grok":
        d = grok_session_dir(session_id, cwd, home)
        return grok_inputs(read_jsonl(d / "chat_history.jsonl"), marker) if d else 0
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


def claude_inputs(records: list[dict], marker: str) -> int:
    return sum(1 for r in records if marker in (_claude_input_text(r) or ""))


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


def codex_inputs(records: list[dict], marker: str) -> int:
    return sum(1 for r in records if marker in (_codex_user_text(r) or ""))


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


def _grok_is_prompt(rec: dict) -> bool:
    """A prompt record carries a `prompt_index`; a mid-turn user record
    (`synthetic_reason`, no `prompt_index`) is not an input."""
    if rec.get("type") != "user":
        return False
    index = rec.get("prompt_index")
    return isinstance(index, int) and not isinstance(index, bool)


def grok_inputs(history: list[dict], marker: str) -> int:
    return sum(1 for r in history if _grok_is_prompt(r) and marker in _grok_text(r))
