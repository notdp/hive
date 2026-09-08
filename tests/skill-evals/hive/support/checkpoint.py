#!/usr/bin/env python3
"""Offline tool-boundary event wrapper; native runners may inject the same event."""
import fcntl
import json
import os
from pathlib import Path
import sys

key = sys.argv[1] if len(sys.argv) == 2 else ''
fixture = json.loads(Path(__file__).with_name('fixture.json').read_text())
event = fixture.get('events', {}).get(key)
print('checkpoint completed: ' + key, flush=True)
if event:
    path = Path(os.environ['HIVE_EVAL_RUN']) / 'runtime-events.jsonl'
    with path.open('a+') as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        stream.seek(0)
        seen = [json.loads(line)['key'] for line in stream if line.strip()]
        if key not in seen:
            stream.write(json.dumps({'key': key, 'envelope': event}) + '\n')
            stream.flush()
            # The data/helper stdout contains no envelope. This wrapper places
            # a bare envelope at the tool-result boundary on a separate stream.
            print(event, file=sys.stderr, flush=True)
