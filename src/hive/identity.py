"""Session/thread/pid identity — not TMUX_PANE.

Tool subprocesses often do not sit on the agent TUI's pty (Codex/Grok
leaders are detached). Hive used to overwrite TMUX_PANE on those daemons
so child `hive` commands could find themselves. That freezes the first
pane's id onto every later client of a shared backend.

This store maps durable ids (session, thread, leader pid) to a pane.
`pane_from_caller` prefers those maps; TMUX_PANE is only trusted when
tmux itself set it.
"""

from __future__ import annotations

import json
import os
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any


def _hive_home() -> Path:
    return Path(os.environ.get("HIVE_HOME", str(Path.home() / ".hive")))


def store_path() -> Path:
    return _hive_home() / "identity.json"


@dataclass(frozen=True)
class Binding:
    pane_id: str
    cli: str = ""
    session_id: str = ""
    thread_id: str = ""
    team: str = ""
    agent: str = ""
    pid: int | None = None


def _empty() -> dict[str, Any]:
    return {"sessions": {}, "threads": {}, "pids": {}}


def _load() -> dict[str, Any]:
    path = store_path()
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return _empty()
    if not isinstance(data, dict):
        return _empty()
    for key in ("sessions", "threads", "pids"):
        if not isinstance(data.get(key), dict):
            data[key] = {}
    return data


def _save(data: dict[str, Any]) -> None:
    path = store_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    tmp.replace(path)


def _row(binding: Binding) -> dict[str, Any]:
    payload = asdict(binding)
    if payload["pid"] is None:
        payload.pop("pid")
    return payload


def _binding(row: Any) -> Binding | None:
    if not isinstance(row, dict):
        return None
    pane = str(row.get("pane_id") or "").strip()
    if not pane:
        return None
    pid_raw = row.get("pid")
    pid = int(pid_raw) if isinstance(pid_raw, int) or (isinstance(pid_raw, str) and pid_raw.isdigit()) else None
    return Binding(
        pane_id=pane,
        cli=str(row.get("cli") or ""),
        session_id=str(row.get("session_id") or ""),
        thread_id=str(row.get("thread_id") or ""),
        team=str(row.get("team") or ""),
        agent=str(row.get("agent") or ""),
        pid=pid,
    )


def bind(binding: Binding) -> Binding:
    data = _load()
    row = _row(binding)
    if binding.session_id:
        data["sessions"][binding.session_id] = row
    if binding.thread_id:
        data["threads"][binding.thread_id] = row
    if binding.pid is not None:
        data["pids"][str(binding.pid)] = row
    _save(data)
    return binding


def lookup_session(session_id: str) -> Binding | None:
    if not session_id:
        return None
    return _binding(_load()["sessions"].get(session_id))


def lookup_thread(thread_id: str) -> Binding | None:
    if not thread_id:
        return None
    return _binding(_load()["threads"].get(thread_id))


def lookup_pid(pid: int) -> Binding | None:
    return _binding(_load()["pids"].get(str(pid)))


def drop_pid(pid: int) -> None:
    data = _load()
    if data["pids"].pop(str(pid), None) is not None:
        _save(data)


def _ancestor_pids() -> list[int]:
    pids: list[int] = []
    pid = os.getpid()
    for _ in range(12):
        stat = Path(f"/proc/{pid}/stat")
        # macOS has no /proc; fall back to ps
        if not stat.exists():
            break
        try:
            text = stat.read_text()
            ppid = int(text.split(")")[-1].split()[1])
        except (OSError, ValueError, IndexError):
            break
        if ppid <= 1 or ppid in pids:
            break
        pids.append(ppid)
        pid = ppid
    if pids:
        return pids
    try:
        import subprocess

        out = subprocess.run(
            ["ps", "-o", "ppid=", "-p", str(os.getpid())],
            capture_output=True,
            text=True,
            check=False,
        )
        seen: list[int] = []
        current = os.getpid()
        for _ in range(12):
            out = subprocess.run(
                ["ps", "-o", "ppid=", "-p", str(current)],
                capture_output=True,
                text=True,
                check=False,
            )
            raw = (out.stdout or "").strip()
            if not raw.isdigit():
                break
            ppid = int(raw)
            if ppid <= 1 or ppid in seen:
                break
            seen.append(ppid)
            current = ppid
        return seen
    except OSError:
        return []


def pane_from_caller() -> str | None:
    """Resolve the calling tool process to a hive pane without forging TMUX_PANE."""
    thread = os.environ.get("CODEX_THREAD_ID", "").strip()
    if thread:
        hit = lookup_thread(thread)
        if hit:
            return hit.pane_id
    session = (
        os.environ.get("GROK_SESSION_ID", "").strip()
        or os.environ.get("HIVE_SESSION_ID", "").strip()
    )
    if session:
        hit = lookup_session(session)
        if hit:
            return hit.pane_id
    for pid in _ancestor_pids():
        hit = lookup_pid(pid)
        if hit:
            return hit.pane_id
    return None
