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
        return f"{BOLD}❯{RESET} {_clip(content)}" if row["type"] == "user" else f"⏺ {_clip(content)}"
    if not isinstance(content, list):
        return None
    parts: list[str] = []
    for block in content:
        if not isinstance(block, dict):
            continue
        kind = block.get("type")
        if kind == "text" and block.get("text", "").strip():
            parts.append(_clip(block["text"]))
        elif kind == "tool_use":
            args = json.dumps(block.get("input", {}), ensure_ascii=False)
            parts.append(f"{CYAN}{block.get('name')}{RESET}({_clip(args, 200)})")
        elif kind == "tool_result":
            body = block.get("content")
            text = body if isinstance(body, str) else json.dumps(body, ensure_ascii=False)
            parts.append(f"{DIM}⎿ {_clip(text, 300)}{RESET}")
    if not parts:
        return None
    marker = f"{BOLD}❯{RESET}" if row["type"] == "user" else "⏺"
    return f"{marker} " + f"\n{DIM}·{RESET} ".join(parts)


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
