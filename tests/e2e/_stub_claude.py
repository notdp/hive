#!/usr/bin/env python3
"""A `claude` stand-in for the e2e suite: the surface hive's claude bg
adapter drives, and nothing else.

Installed first on PATH as `claude` by test_spawn_send_kill_with_a_stub_cli.
Every invocation appends its argv to `$CLAUDE_CONFIG_DIR/stub/argv.jsonl`
(the recorded-argv oracle). Subcommands:

- `--bg [--name N] [--model M] [PROMPT]`: mint a job id, fork an engine
  process, answer `backgrounded · <jobId>` like the real CLI.
- `agents --json --all`: the job ledger.
- `attach <jobId>`: block like the real client until stdin reaches EOF
  (hive closes the pipe when it is done typing).
- `stop <jobId>`: park the job (SIGTERM its engine).
- `rm <jobId>`: drop the ledger row.

The engine registers itself the way a real bg engine does — a
`kind: "bg"` entry under `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` naming a
live unix socket — and publishes that identity for the test under
`stub/engine-<jobId>.json`. Every frame hive writes into the socket (the
inbox delivery lane) is journaled to `stub/inbox-<jobId>.jsonl`. An engine
minted under a `<team>.<member>` label with a prompt then behaves like a
member at birth: it asks `hive team` who it is until hive answers with its
own name, sends the nonce on the last line of its prompt to orch with
`hive send`, records that send's outcome under `stub/send-<jobId>.json`,
and idles until parked. Any other engine (an orch's) only idles: the human
drives it.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

CFG = Path(os.environ["CLAUDE_CONFIG_DIR"])
STUB = CFG / "stub"
JOBS = STUB / "jobs.json"
SELF_READY_TIMEOUT = 60.0


def _record_argv() -> None:
    STUB.mkdir(parents=True, exist_ok=True)
    with (STUB / "argv.jsonl").open("a") as fh:
        fh.write(json.dumps({"pid": os.getpid(), "argv": sys.argv[1:]}) + "\n")


def _load_jobs() -> list[dict]:
    if not JOBS.exists():
        return []
    return json.loads(JOBS.read_text())


def _save_jobs(rows: list[dict]) -> None:
    tmp = JOBS.with_suffix(".tmp")
    tmp.write_text(json.dumps(rows))
    os.replace(tmp, JOBS)


def _bg(args: list[str]) -> int:
    name = ""
    prompt = ""
    consume_next = {"--name", "--model", "-r", "--resume", "--settings"}
    i = 0
    while i < len(args):
        arg = args[i]
        if arg in consume_next:
            if arg == "--name" and i + 1 < len(args):
                name = args[i + 1]
            i += 2
            continue
        if arg.startswith("-"):
            i += 1
            continue
        prompt = arg
        i += 1
    job_id = uuid.uuid4().hex[:8]
    session_id = str(uuid.uuid4())
    STUB.mkdir(parents=True, exist_ok=True)
    log = (STUB / f"engine-{job_id}.log").open("ab")
    engine = subprocess.Popen(
        [sys.executable, os.path.realpath(__file__), "__engine__", job_id, session_id, name, os.getcwd()],
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=log,
        start_new_session=True,
        env={**os.environ, "HIVE_STUB_PROMPT": prompt},
    )
    rows = _load_jobs()
    rows.append({
        "id": job_id,
        "name": name,
        "sessionId": session_id,
        "cwd": os.getcwd(),
        "status": "running",
        "pid": engine.pid,
    })
    _save_jobs(rows)
    print(f"backgrounded · {job_id}")
    return 0


def _stop(job_id: str) -> int:
    rows = _load_jobs()
    for row in rows:
        if row["id"] != job_id:
            continue
        pid = row.pop("pid", None)
        row.pop("status", None)
        if pid:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    _save_jobs(rows)
    return 0


def _inbox(server: socket.socket, journal: Path) -> None:
    while True:
        conn, _ = server.accept()
        with conn:
            raw = b""
            while chunk := conn.recv(65536):
                raw += chunk
        for line in raw.decode(errors="replace").splitlines():
            if line.strip():
                with journal.open("a") as fh:
                    fh.write(line + "\n")


def _engine(job_id: str, session_id: str, name: str, cwd: str) -> None:
    pid = os.getpid()
    sock_dir = CFG / "cc-socks"
    sock_dir.mkdir(parents=True, exist_ok=True)
    sock_path = sock_dir / f"{pid}.sock"
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(sock_path))
    server.listen(8)
    threading.Thread(target=_inbox, args=(server, STUB / f"inbox-{job_id}.jsonl"), daemon=True).start()

    entry = CFG / "sessions" / f"{pid}.json"
    entry.parent.mkdir(parents=True, exist_ok=True)
    entry.write_text(json.dumps({
        "kind": "bg",
        "pid": pid,
        "jobId": job_id,
        "sessionId": session_id,
        "messagingSocketPath": str(sock_path),
        "cwd": cwd,
        "status": "idle",
        "statusUpdatedAt": int(time.time() * 1000),
        "name": name,
    }))

    (STUB / f"engine-{job_id}.json").write_text(json.dumps({
        "pid": pid,
        "sessionId": session_id,
        "socket": str(sock_path),
    }))

    def _park(*_args) -> None:
        entry.unlink(missing_ok=True)
        sock_path.unlink(missing_ok=True)
        os._exit(0)

    signal.signal(signal.SIGTERM, _park)

    # A bg engine's tools run outside any tmux client; the socket is the
    # engine's only identity marker.
    env = dict(os.environ)
    env.pop("TMUX", None)
    env.pop("TMUX_PANE", None)
    env.pop("HIVE_STUB_PROMPT", None)
    env["CLAUDE_CODE_MESSAGING_SOCKET"] = str(sock_path)
    prompt_lines = [line for line in os.environ.get("HIVE_STUB_PROMPT", "").splitlines() if line.strip()]
    nonce = prompt_lines[-1].strip() if prompt_lines else ""
    if "." not in name or not nonce:
        while True:
            time.sleep(3600)
    member = name.split(".", 1)[1]

    outcome = {"job": job_id, "nonce": nonce, "identity": None, "send": None}
    deadline = time.time() + SELF_READY_TIMEOUT
    while time.time() < deadline:
        team = subprocess.run(["hive", "team"], env=env, capture_output=True, text=True, timeout=30)
        if team.returncode == 0:
            try:
                payload = json.loads(team.stdout)
            except ValueError:
                payload = {}
            names = [m.get("name") for m in payload.get("members", [])]
            if payload.get("self") == member and member in names:
                outcome["identity"] = {"self": payload.get("self"), "members": names}
                break
        time.sleep(0.2)
    if outcome["identity"] is not None:
        send = subprocess.run(["hive", "send", "orch", nonce], env=env, capture_output=True, text=True, timeout=60)
        outcome["send"] = {"rc": send.returncode, "stdout": send.stdout, "stderr": send.stderr}
    (STUB / f"send-{job_id}.json").write_text(json.dumps(outcome))
    while True:
        time.sleep(3600)


def main() -> int:
    args = sys.argv[1:]
    if args[:1] == ["__engine__"]:
        _engine(*args[1:5])
        return 0
    _record_argv()
    if not args:
        return 2
    if args[0] == "--bg":
        return _bg(args[1:])
    if args[0] == "agents":
        print(json.dumps(_load_jobs()))
        return 0
    if args[0] == "attach":
        # The real client exits when its stdin closes; hive ends the keyboard
        # lane exactly that way, so anything else here leaks a process.
        try:
            while sys.stdin.buffer.read(65536):
                pass
        except OSError:
            pass
        return 0
    if args[0] == "stop" and len(args) > 1:
        return _stop(args[1])
    if args[0] == "rm" and len(args) > 1:
        _save_jobs([row for row in _load_jobs() if row["id"] != args[1]])
        return 0
    if args[0] in ("-v", "--version"):
        print("0.0.0 (hive e2e stub)")
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
