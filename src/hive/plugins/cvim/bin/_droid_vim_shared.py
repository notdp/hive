from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path


_RESUME_RE = re.compile(r"--resume\s+([0-9a-fA-F-]{36})")


def session_dir_name(path: str) -> str:
    return path.replace("/", "-") or "-"


def sessions_root() -> Path:
    return Path(os.environ.get("FACTORY_HOME", str(Path.home() / ".factory"))) / "sessions"


def iter_candidate_files(path: str):
    seen: set[str] = set()
    root = sessions_root()
    for candidate in [path, os.path.realpath(path)]:
        if not candidate or candidate in seen:
            continue
        seen.add(candidate)
        session_dir = root / session_dir_name(candidate)
        if session_dir.is_dir():
            yield from sorted(session_dir.glob("*.jsonl"), key=lambda p: p.stat().st_mtime_ns, reverse=True)
    if root.is_dir():
        yield from sorted(root.glob("*.jsonl"), key=lambda p: p.stat().st_mtime_ns, reverse=True)


def extract_last_assistant_text(file_path: Path, offset: int = 0) -> str:
    """Return the Nth assistant message from the end (0=last, 1=second-to-last, ...)."""
    try:
        lines = file_path.read_text(errors="ignore").splitlines()
    except OSError:
        return ""
    skip = offset
    for line in reversed(lines):
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if obj.get("type") != "message":
            continue
        message = obj.get("message") or {}
        if message.get("role") != "assistant":
            continue
        parts: list[str] = []
        fallback_plan = ""
        for item in message.get("content") or []:
            if item.get("type") == "text":
                text = item.get("text") or ""
                if text.strip():
                    parts.append(text.rstrip("\n"))
            elif item.get("type") == "tool_use" and item.get("name") == "ExitSpecMode":
                tool_input = item.get("input") or {}
                plan = tool_input.get("plan") if isinstance(tool_input, dict) else ""
                if isinstance(plan, str) and plan.strip():
                    fallback_plan = plan.strip()
        text_result = ""
        if parts:
            text_result = "\n\n".join(parts).strip()
        elif fallback_plan:
            text_result = fallback_plan
        if text_result:
            if skip <= 0:
                return text_result
            skip -= 1
    return ""


def write_seed(cwd: str, dst: Path, preferred: Path | None = None, offset: int = 0) -> None:
    if preferred is not None:
        text = extract_last_assistant_text(preferred, offset=offset)
        dst.write_text(text + "\n" if text else "")
        return

    for index, file_path in enumerate(iter_candidate_files(cwd)):
        if index >= 10:
            break
        text = extract_last_assistant_text(file_path, offset=offset)
        if text:
            dst.write_text(text + "\n")
            return
    dst.write_text("")


def extract_resume_session_id(args: str) -> str | None:
    match = _RESUME_RE.search(args or "")
    if not match:
        return None
    return match.group(1)


def load_session_map(session_map_file: str | Path) -> dict[str, object]:
    try:
        return json.loads(Path(session_map_file).read_text())
    except Exception:
        return {}


def lookup_map_transcript(session_map: dict[str, object], *, pid: str = "", tty: str = "") -> Path | None:
    for bucket, key in (("by_pid", pid), ("by_tty", tty)):
        if not key:
            continue
        record = (session_map.get(bucket) or {}).get(key)
        if not isinstance(record, dict):
            continue
        transcript_path = record.get("transcript_path")
        if not isinstance(transcript_path, str) or not transcript_path:
            continue
        path = Path(transcript_path)
        if path.is_file():
            return path
    return None


def find_resume_transcript(cwd: str, session_id: str) -> Path | None:
    root = sessions_root()
    seen: set[str] = set()
    for candidate in [cwd, os.path.realpath(cwd)]:
        if not candidate or candidate in seen:
            continue
        seen.add(candidate)
        file_path = root / session_dir_name(candidate) / f"{session_id}.jsonl"
        if file_path.is_file():
            return file_path
    for file_path in root.glob(f"**/{session_id}.jsonl"):
        if file_path.is_file():
            return file_path
    return None


def resolve_transcript_path(
    *, session_map_file: str | Path, cwd: str, pid: str = "", tty: str = "", droid_args: str = ""
) -> str | None:
    transcript_path = lookup_map_transcript(load_session_map(session_map_file), pid=pid, tty=tty)
    if transcript_path is not None:
        return str(transcript_path)

    resume_session_id = extract_resume_session_id(droid_args)
    if resume_session_id:
        transcript_path = find_resume_transcript(cwd, resume_session_id)
        if transcript_path is not None:
            return str(transcript_path)
    return None


def resolve_current_droid_process_info(root_pid: int, pane_tty: str) -> tuple[str, str, str] | None:
    out = subprocess.check_output(["ps", "-axo", "pid=,ppid=,tty=,comm=,args="], text=True)
    rows: list[tuple[int, int, str, str, str]] = []
    for line in out.splitlines():
        parts = line.strip().split(None, 4)
        if len(parts) < 5:
            continue
        pid, ppid, tty, comm, args = int(parts[0]), int(parts[1]), parts[2], parts[3], parts[4]
        rows.append((pid, ppid, tty, comm, args))

    by_ppid: dict[int, list[tuple[int, int, str, str, str]]] = {}
    for row in rows:
        by_ppid.setdefault(row[1], []).append(row)

    best: tuple[int, int, str, str, str] | None = None
    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for child in by_ppid.get(pid, []):
            child_pid, _, tty, comm, args = child
            if comm == "droid":
                if best is None or tty == pane_tty:
                    best = child
                    if tty == pane_tty:
                        return (str(child_pid), tty, args)
            stack.append(child_pid)

    if best is None:
        return None
    return (str(best[0]), best[2], best[4])


def capture_session_seed(
    *,
    cwd: str,
    dst: Path,
    session_map_file: str | Path,
    pid: str = "",
    tty: str = "",
    droid_args: str = "",
    offset: int = 0,
) -> None:
    transcript_path = resolve_transcript_path(
        session_map_file=session_map_file,
        cwd=cwd,
        pid=pid,
        tty=tty,
        droid_args=droid_args,
    )
    write_seed(cwd, dst, Path(transcript_path) if transcript_path else None, offset=offset)
