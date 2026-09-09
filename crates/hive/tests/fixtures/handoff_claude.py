#!/usr/bin/env python3
"""A process/tty fixture, not an LLM: one persistent engine and its viewers."""
import fcntl
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import termios
import time
import tty

root = Path(os.environ['HANDOFF_TEST_ROOT'])
config = Path(os.environ['CLAUDE_CONFIG_DIR'])
job = 'abc12345'

def event(kind, **fields):
    with (root / 'events').open('a') as f:
        f.write(json.dumps(dict(kind=kind, pid=os.getpid(), **fields)) + '\n')

mode = sys.argv[1] if len(sys.argv) > 1 else ''
if mode == '--bg':
    subprocess.Popen([sys.executable, __file__, 'engine'], stdin=subprocess.DEVNULL,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    for _ in range(200):
        if (root / 'engine.json').exists():
            print('backgrounded · ' + job)
            sys.exit(0)
        time.sleep(.01)
    sys.exit(1)
elif mode == 'engine':
    config.joinpath('sessions').mkdir(parents=True, exist_ok=True)
    sock = socket.socket(socket.AF_UNIX)
    path = str(root / f'{os.getpid()}.sock')
    sock.bind(path)
    sock.listen()
    data = dict(pid=os.getpid(), name='handoff-fixture', kind='bg', jobId=job,
                sessionId=job+'-ffff-4aaa-8bbb-000000000000', messagingSocketPath=path,
                entrypoint='cli', cwd=str(root), status='busy', statusUpdatedAt=time.time()*1000)
    config.joinpath('sessions', f'{os.getpid()}.json').write_text(json.dumps(data))
    root.joinpath('engine.json').write_text(json.dumps(data))
    while True:
        # Persistent work continues while no viewer is attached.
        root.joinpath('engine-tick').write_text(str(time.monotonic()))
        if select.select([sock], [], [], .05)[0]:
            peer, _ = sock.accept()
            peer.close()
elif mode == 'agents':
    data = json.loads(root.joinpath('engine.json').read_text())
    print(json.dumps([dict(id=job, pid=data['pid'], status='busy', name='handoff-fixture')]))
elif mode == 'stop':
    event('stop')
    data = json.loads(root.joinpath('engine.json').read_text())
    try:
        os.kill(data['pid'], signal.SIGTERM)
    except ProcessLookupError:
        pass
elif mode == 'attach':
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
        event('attach', pane=os.environ.get('TMUX_PANE', ''), roots={k:os.environ.get(k) for k in
              ('HIVE_HOME','CLAUDE_HOME','CLAUDE_CONFIG_DIR','CODEX_HOME','GROK_HOME')})
    modes = termios.tcgetattr(0)
    tty.setraw(0)
    print('fixture conversation ' + job, flush=True)
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
    event('other', argv=sys.argv[1:])
