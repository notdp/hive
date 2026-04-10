from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any


_RESUME_RE = re.compile(r"--resume\s+([0-9a-fA-F-]{36})")
_CODEX_COMMAND_SKILL_RE = re.compile(r"^\s*\$(?:cvim|vim)(?:\s|$)")


def session_dir_name(path: str) -> str:
    return path.replace("/", "-") or "-"


def sessions_root() -> Path:
    return Path(os.environ.get("FACTORY_HOME", str(Path.home() / ".factory"))) / "sessions"


def _available_adapter_names() -> list[str]:
    from hive import adapters as hive_adapters

    return hive_adapters.available()


def _get_adapter(name: str):
    from hive import adapters as hive_adapters

    return hive_adapters.get(name)


def _detect_profile_for_pane(pane_id: str):
    from hive.agent_cli import detect_profile_for_pane

    return detect_profile_for_pane(pane_id)


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
    adapter = _detect_adapter_for_transcript(file_path)
    if adapter is not None:
        return _extract_last_assistant_text_via_adapter(
            adapter,
            file_path,
            offset=resolve_assistant_offset(file_path, offset=offset, adapter=adapter),
        )

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
        for item in message.get("content") or []:
            if item.get("type") == "text":
                text = item.get("text") or ""
                if text.strip():
                    parts.append(text.rstrip("\n"))
            elif item.get("type") == "tool_use" and item.get("name") == "ExitSpecMode":
                tool_input = item.get("input") or {}
                plan = tool_input.get("plan") if isinstance(tool_input, dict) else ""
                title = tool_input.get("title") if isinstance(tool_input, dict) else ""
                if isinstance(plan, str) and plan.strip():
                    header = ""
                    if isinstance(title, str) and title.strip():
                        header = f'Propose Specification title: "{title.strip()}"\n\n'
                    parts.append(f"{header}Specification for approval:\n\n{plan.strip()}")
        text_result = "\n\n".join(parts).strip() if parts else ""
        if text_result:
            if skip <= 0:
                return text_result
            skip -= 1
    return ""


def _detect_adapter_for_transcript(file_path: Path):
    try:
        for name in _available_adapter_names():
            adapter = _get_adapter(name)
            if adapter is None:
                continue
            try:
                meta = adapter.read_meta(file_path)
            except Exception:
                meta = None
            if meta is not None:
                return adapter
    except Exception:
        return None
    return None


def _extract_last_assistant_text_via_adapter(adapter, file_path: Path, *, offset: int = 0) -> str:
    skip = offset
    try:
        messages = list(adapter.iter_messages(file_path))
    except Exception:
        return ""
    for message in reversed(messages):
        if getattr(message, "role", "") != "assistant":
            continue
        text_result = _assistant_text_from_normalized_message(message)
        if text_result:
            if skip <= 0:
                return text_result
            skip -= 1
    return ""


def resolve_assistant_offset(file_path: Path, offset: int = 0, *, adapter=None) -> int:
    if adapter is None:
        adapter = _detect_adapter_for_transcript(file_path)
    if adapter is None or getattr(adapter, "name", "") != "codex":
        return offset
    try:
        messages = list(adapter.iter_messages(file_path))
    except Exception:
        return offset
    return _resolve_codex_skill_turn_offset(messages, offset=offset)


def _resolve_codex_skill_turn_offset(messages: list[Any], *, offset: int = 0) -> int:
    tail_turn_id = None
    for message in reversed(messages):
        turn_id = _message_turn_id(message)
        if turn_id:
            tail_turn_id = turn_id
            break
    if not tail_turn_id:
        return offset

    tail_turn = [message for message in messages if _message_turn_id(message) == tail_turn_id]
    if not _turn_invokes_codex_command_skill(tail_turn):
        return offset

    synthetic_assistant_messages = sum(
        1 for message in tail_turn if _is_codex_commentary_assistant_message(message)
    )
    return offset + synthetic_assistant_messages


def _message_turn_id(message: Any) -> str | None:
    raw = getattr(message, "raw", None)
    if not isinstance(raw, dict):
        return None
    turn_id = raw.get("turn_id")
    return turn_id if isinstance(turn_id, str) and turn_id else None


def _turn_invokes_codex_command_skill(messages: list[Any]) -> bool:
    for message in messages:
        if getattr(message, "role", "") != "user":
            continue
        for item in getattr(message, "parts", ()) or ():
            if getattr(item, "kind", "") != "text":
                continue
            text = getattr(item, "text", "") or ""
            if isinstance(text, str) and _CODEX_COMMAND_SKILL_RE.match(text):
                return True
    return False


def _is_codex_commentary_assistant_message(message: Any) -> bool:
    if getattr(message, "role", "") != "assistant":
        return False
    raw = getattr(message, "raw", None)
    if not isinstance(raw, dict):
        return False
    payload = raw.get("payload")
    if not isinstance(payload, dict):
        return False
    return payload.get("type") == "message" and payload.get("phase") == "commentary"


def _assistant_text_from_normalized_message(message: Any) -> str:
    parts: list[str] = []
    for item in getattr(message, "parts", ()) or ():
        kind = getattr(item, "kind", "")
        if kind == "text":
            text = getattr(item, "text", "") or ""
            if isinstance(text, str) and text.strip():
                parts.append(text.rstrip("\n"))
        elif kind == "tool_use" and getattr(item, "tool_name", "") == "ExitSpecMode":
            tool_input = getattr(item, "tool_input", None) or {}
            if not isinstance(tool_input, dict):
                continue
            plan = tool_input.get("plan")
            title = tool_input.get("title")
            if isinstance(plan, str) and plan.strip():
                header = ""
                if isinstance(title, str) and title.strip():
                    header = f'Propose Specification title: "{title.strip()}"\n\n'
                parts.append(f"{header}Specification for approval:\n\n{plan.strip()}")
    return "\n\n".join(parts).strip() if parts else ""


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


def resolve_transcript_path_for_pane(
    *,
    pane_id: str,
    session_map_file: str | Path,
    cwd: str,
    pid: str = "",
    tty: str = "",
    droid_args: str = "",
) -> str | None:
    if pane_id:
        try:
            profile = _detect_profile_for_pane(pane_id)
        except Exception:
            profile = None
        if profile is not None:
            adapter = _get_adapter(profile.name)
            if adapter is not None:
                try:
                    session_id = adapter.resolve_current_session_id(pane_id)
                except Exception:
                    session_id = None
                if session_id:
                    try:
                        transcript_path = adapter.find_session_file(session_id, cwd=cwd)
                    except Exception:
                        transcript_path = None
                    if transcript_path is not None and Path(transcript_path).is_file():
                        return str(transcript_path)
            if profile.name != "droid":
                return None
    return resolve_transcript_path(
        session_map_file=session_map_file,
        cwd=cwd,
        pid=pid,
        tty=tty,
        droid_args=droid_args,
    )


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
