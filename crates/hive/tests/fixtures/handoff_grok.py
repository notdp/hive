#!/usr/bin/env python3
"""A process/tty fixture standing in for grok: a leader daemon that survives
its viewers, and the TUIs that attach to it. Not an LLM, no network."""
import fcntl
import json
import os
from pathlib import Path
import select
import signal
import socket
import sys
import termios
import tty

root = Path(os.environ['HANDOFF_TEST_ROOT'])
argv = sys.argv[1:]


def event(kind, **fields):
    with (root / 'events').open('a') as f:
        f.write(json.dumps(dict(kind=kind, pid=os.getpid(), **fields)) + '\n')


def opt(name):
    return argv[argv.index(name) + 1] if name in argv else ''


if argv[:2] == ['agent', 'leader']:
    path = opt('--leader-socket')
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    sock = socket.socket(socket.AF_UNIX)
    sock.bind(path)
    sock.listen()
    root.joinpath('leader.json').write_text(json.dumps(dict(pid=os.getpid(), socket=path)))

    def stop(*_):
        event('stop', socket=path)
        sys.exit(0)
    signal.signal(signal.SIGTERM, stop)
    while True:
        if select.select([sock], [], [], .05)[0]:
            peer, _ = sock.accept()
            peer.close()
elif argv[:1] == ['agent']:
    sys.exit(1)  # hive's own stdio client: no leader protocol here
elif '--leader' in argv:
    if not os.isatty(0):
        sys.exit(0)
    active = root / 'viewer.pid'
    with root.joinpath('viewer.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if active.exists():
            try:
                previous = int(active.read_text())
                os.kill(previous, 0)
                event('overlap', previous=previous)
            except (ValueError, ProcessLookupError):
                pass
        active.write_text(str(os.getpid()))
        event('attach', pane=os.environ.get('TMUX_PANE', ''), socket=opt('--leader-socket'),
              session=opt('--session-id') or opt('--resume'),
              roots={k: os.environ.get(k) for k in ('HIVE_HOME', 'CLAUDE_HOME', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME', 'GROK_HOME')})
    modes = termios.tcgetattr(0)
    tty.setraw(0)
    print('fixture grok ' + (opt('--session-id') or opt('--resume')), flush=True)
    try:
        while True:
            key = os.read(0, 1)
            if not key or key == b'q':
                break
            event('input', key=key.decode(errors='replace'))
    finally:
        termios.tcsetattr(0, termios.TCSANOW, modes)
        if active.exists() and active.read_text() == str(os.getpid()):
            active.unlink()
else:
    event('other', argv=argv)
