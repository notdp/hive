"""Read-only live mirror for a Claude session transcript.

An interactive Claude session (a desktop ccd, a joined session) has no
attachable pty — `claude attach` is job-only, and resuming would fork a
second engine. Its truth layer is the transcript JSONL, appended event by
event as the turn unfolds, so a faithful renderer over that file IS the
mirror: native-looking, keystrokes go nowhere by construction.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import time
from pathlib import Path

RESET = "\x1b[0m"
BOLD = "\x1b[1m"
DIM = "\x1b[2m"
CYAN = "\x1b[36m"
GREEN = "\x1b[32m"
MAGENTA = "\x1b[35m"
YELLOW = "\x1b[33m"
CLEAR_LINE = "\x1b[2K\r"

_TAIL_EVENTS = 40
_POLL_SECONDS = 0.25
_SPINNER = "✻✼✢✽"
_HIVE_RE = re.compile(r"<HIVE\s+from=(\S+)[^>]*>\s*(.*?)\s*</HIVE>", re.S)
_MD_BOLD = re.compile(r"\*\*(.+?)\*\*")
_MD_CODE = re.compile(r"`([^`\n]+)`")


def external_viewer() -> str | None:
    """Path to tail-claude when installed — the preferred renderer.

    tail-claude (github.com/kylesnowschwartz/tail-claude) live-tails a
    session JSONL as a full conversation TUI; the built-in renderer below
    is the dependency-free fallback.
    """
    found = shutil.which("tail-claude")
    if found:
        return found
    candidate = Path.home() / "go" / "bin" / "tail-claude"
    return str(candidate) if candidate.is_file() and os.access(candidate, os.X_OK) else None


def transcript_path(session_id: str) -> Path | None:
    home = Path.home() / ".claude" / "projects"
    matches = sorted(
        home.glob(f"*/{session_id}.jsonl"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    return matches[0] if matches else None


def _clip(text: str, limit: int) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[:limit] + " …"


def _md(text: str) -> str:
    """Just enough markdown for a terminal: bold and inline code."""
    text = _MD_BOLD.sub(f"{BOLD}\\1{RESET}", text)
    return _MD_CODE.sub(f"{CYAN}\\1{RESET}", text)


def _indent_block(text: str, first: str, rest: str = "  ") -> str:
    lines = text.splitlines() or [""]
    out = [f"{first}{lines[0]}"]
    out.extend(f"{rest}{line}" for line in lines[1:])
    return "\n".join(out)


def _tool_line(block: dict) -> str:
    name = block.get("name", "?")
    inp = block.get("input", {}) or {}
    hint = (
        inp.get("description")
        or inp.get("file_path")
        or inp.get("command")
        or inp.get("prompt")
        or json.dumps(inp, ensure_ascii=False)
    )
    hint = _clip(str(hint).splitlines()[0] if str(hint) else "", 140)
    return f"{GREEN}⏺{RESET} {BOLD}{name}{RESET}({CYAN}{hint}{RESET})"


def _user_line(text: str) -> str:
    envelope = _HIVE_RE.search(text)
    if envelope:
        sender, body = envelope.group(1), _clip(envelope.group(2), 160)
        return f"{MAGENTA}✉{RESET} {BOLD}{sender}{RESET} {DIM}▸{RESET} {body}"
    return _indent_block(_clip(text, 1200), f"{BOLD}❯{RESET} {BOLD}", "  ") + RESET


class _Renderer:
    """Fold transcript rows into printed lines and a liveness verdict."""

    def __init__(self) -> None:
        self.tokens = 0
        self.state = "idle"  # idle | working
        self.state_since = time.monotonic()

    def _set_state(self, state: str) -> None:
        if state != self.state:
            self.state = state
            self.state_since = time.monotonic()

    def render(self, raw: str) -> str | None:
        try:
            row = json.loads(raw)
        except ValueError:
            return None
        kind = row.get("type")
        if kind not in ("user", "assistant"):
            return None
        message = row.get("message") or {}
        usage = message.get("usage") or {}
        self.tokens += int(usage.get("output_tokens") or 0)
        content = message.get("content")
        blocks = (
            [{"type": "text", "text": content}]
            if isinstance(content, str)
            else content if isinstance(content, list) else []
        )
        lines: list[str] = []
        saw_tool_use = False
        saw_text = False
        for block in blocks:
            if not isinstance(block, dict):
                continue
            btype = block.get("type")
            if btype == "text" and str(block.get("text", "")).strip():
                body = str(block["text"]).strip()
                if kind == "user":
                    lines.append("\n" + _user_line(body))
                else:
                    saw_text = True
                    lines.append("\n" + _indent_block(_md(_clip(body, 4000)), "⏺ "))
            elif btype == "tool_use":
                saw_tool_use = True
                lines.append("\n" + _tool_line(block))
            elif btype == "tool_result":
                body = block.get("content")
                text = body if isinstance(body, str) else json.dumps(body, ensure_ascii=False)
                first = _clip(text, 160).splitlines()[0] if str(text).strip() else ""
                if first:
                    lines.append(f"  {DIM}⎿  {first}{RESET}")
        if kind == "user":
            self._set_state("working")
        elif saw_tool_use:
            self._set_state("working")
        elif saw_text:
            self._set_state("idle")
        return "".join(lines) if lines else None

    def status_line(self, tick: int, session_id: str) -> str:
        if self.state == "working":
            frame = _SPINNER[tick % len(_SPINNER)]
            elapsed = int(time.monotonic() - self.state_since)
            verb = f"{YELLOW}{frame}{RESET} Working… {DIM}({elapsed}s){RESET}"
        else:
            verb = f"{GREEN}●{RESET} idle"
        return (
            f"{verb} {DIM}· {session_id[:8]} · {self.tokens} tokens out · "
            f"read-only mirror{RESET}"
        )


def follow(session_id: str) -> int:
    path = transcript_path(session_id)
    if path is None:
        print(f"no transcript for session '{session_id}'")
        return 1
    print(f"{DIM}── live mirror · {path.name} · keys go nowhere ──{RESET}")
    renderer = _Renderer()
    tick = 0
    with open(path, encoding="utf-8") as fh:
        for raw in fh.readlines()[-_TAIL_EVENTS:]:
            rendered = renderer.render(raw)
            if rendered:
                print(rendered, flush=True)
        try:
            while True:
                raw = fh.readline()
                if raw:
                    rendered = renderer.render(raw)
                    if rendered:
                        sys.stdout.write(CLEAR_LINE)
                        print(rendered, flush=True)
                    continue
                tick += 1
                sys.stdout.write(CLEAR_LINE + renderer.status_line(tick, session_id))
                sys.stdout.flush()
                time.sleep(_POLL_SECONDS)
        except KeyboardInterrupt:
            sys.stdout.write(CLEAR_LINE)
            return 0
