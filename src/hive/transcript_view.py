"""Read-only viewer for a Claude session transcript.

An interactive Claude session (a desktop ccd, a joined session) has no
attachable pty — `claude attach` is job-only, and `claude -r` would FORK
the session into a second engine. Its truth layer is the transcript
JSONL, so the viewer renders that file and follows it. Keystrokes go
nowhere by construction.
"""

from __future__ import annotations

import json
import time
from pathlib import Path

DIM = "\x1b[2m"
BOLD = "\x1b[1m"
RESET = "\x1b[0m"
CYAN = "\x1b[36m"

_TAIL_EVENTS = 40
_POLL_SECONDS = 0.5
_BODY_WIDTH = 2000


def transcript_path(session_id: str) -> Path | None:
    home = Path.home() / ".claude" / "projects"
    matches = sorted(
        home.glob(f"*/{session_id}.jsonl"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    return matches[0] if matches else None


def _clip(text: str, limit: int = _BODY_WIDTH) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[:limit] + " …"


GREEN = "\x1b[32m"


def _tool_line(block: dict) -> str:
    name = block.get("name", "?")
    inp = block.get("input", {}) or {}
    # Prefer the human-readable slot each tool carries over raw JSON.
    hint = (
        inp.get("description")
        or inp.get("file_path")
        or inp.get("command")
        or inp.get("prompt")
        or json.dumps(inp, ensure_ascii=False)
    )
    return f"{GREEN}⏺{RESET} {BOLD}{name}{RESET}({CYAN}{_clip(str(hint), 160)}{RESET})"


def _render_event(raw: str) -> str | None:
    try:
        row = json.loads(raw)
    except ValueError:
        return None
    if row.get("type") not in ("user", "assistant"):
        return None
    message = row.get("message") or {}
    content = message.get("content")
    if isinstance(content, str):
        text = _clip(content)
        return f"\n{BOLD}❯ {text}{RESET}" if row["type"] == "user" else f"\n⏺ {text}"
    if not isinstance(content, list):
        return None
    lines: list[str] = []
    for block in content:
        if not isinstance(block, dict):
            continue
        kind = block.get("type")
        if kind == "text" and block.get("text", "").strip():
            body = _clip(block["text"])
            if row["type"] == "user":
                lines.append(f"\n{BOLD}❯ {body}{RESET}")
            else:
                lines.append(f"\n⏺ {body}")
        elif kind == "tool_use":
            lines.append("\n" + _tool_line(block))
        elif kind == "tool_result":
            body = block.get("content")
            text = body if isinstance(body, str) else json.dumps(body, ensure_ascii=False)
            first = _clip(text, 200).splitlines()[0] if text.strip() else ""
            if first:
                lines.append(f"  {DIM}⎿  {first}{RESET}")
    return "".join(lines) if lines else None


def follow(session_id: str) -> int:
    path = transcript_path(session_id)
    if path is None:
        print(f"no transcript for session '{session_id}'")
        return 1
    print(f"{DIM}── read-only viewer · {session_id[:8]} · {path.name} · keys go nowhere ──{RESET}")
    with open(path, encoding="utf-8") as fh:
        lines = fh.readlines()
        for raw in lines[-_TAIL_EVENTS:]:
            rendered = _render_event(raw)
            if rendered:
                print(rendered, flush=True)
        try:
            while True:
                raw = fh.readline()
                if not raw:
                    time.sleep(_POLL_SECONDS)
                    continue
                rendered = _render_event(raw)
                if rendered:
                    print(rendered, flush=True)
        except KeyboardInterrupt:
            return 0
