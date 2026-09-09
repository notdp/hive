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
config = Path(os.environ['CODEX_HOME'])
job = '11111111-2222-4333-8444-555555555555'

def event(kind, **fields):
    with (root / 'events').open('a') as f:
        f.write(json.dumps(dict(kind=kind, pid=os.getpid(), **fields)) + '\n')


import base64, hashlib, struct, threading

def serve(peer):
    try:
        f = peer.makefile('rb')
        headers = {}
        while True:
            line = f.readline()
            if line in (b'\r\n', b''): break
            if b':' in line:
                k,v = line.decode().split(':',1); headers[k.lower()] = v.strip()
        key = headers.get('sec-websocket-key', '')
        accept = base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
        peer.sendall(('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: '+accept+'\r\n\r\n').encode())
        while True:
            h=f.read(2)
            if len(h)!=2 or h[0]&15 == 8: break
            size=h[1]&127
            if size==126:size=struct.unpack('!H',f.read(2))[0]
            elif size==127:size=struct.unpack('!Q',f.read(8))[0]
            mask=f.read(4) if h[1]&128 else None
            data=f.read(size)
            if mask:data=bytes(b^mask[i%4] for i,b in enumerate(data))
            msg=json.loads(data)
            if 'id' not in msg:continue
            method=msg.get('method')
            result={}
            if method in ('thread/start','thread/resume','thread/fork','thread/read'):
                event(method)
                rollout=root/'rollout.jsonl';rollout.touch()
                result={'thread':{'id':job,'path':str(rollout),'status':{'type':'idle'},'cwd':str(root)}}
            elif method == 'account/read':result={'account':None}
            data=json.dumps({'id':msg['id'],'result':result}).encode()
            frame=b'\x81'+(bytes([len(data)]) if len(data)<126 else b'\x7e'+struct.pack('!H',len(data)))+data
            peer.sendall(frame)
    except (OSError, ValueError):pass
    finally:peer.close()

args=sys.argv[1:]
if args and args[0]=='app-server':
    path=args[args.index('--listen')+1].removeprefix('unix://')
    sock=socket.socket(socket.AF_UNIX);sock.bind(path);sock.listen()
    root.joinpath('engine.json').write_text(json.dumps({'pid':os.getpid(),'sessionId':job}))
    while True:
        peer,_=sock.accept()
        threading.Thread(target=serve,args=(peer,),daemon=True).start()
elif 'resume' in args:
    # Match the npm wrapper: the native viewer is a child in the same pgrp.
    pid=os.fork()
    if pid:
        _,status=os.waitpid(pid,0);sys.exit(os.waitstatus_to_exitcode(status))
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
    event('other', argv=args)
