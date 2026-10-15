#!/usr/bin/env python3
"""OFFLINE test process. Not Claude, not an SDK, never contacts a network.

A tiny line-protocol peer lets the real adapter/owner tests observe OS process
lifetimes and native-like session persistence, rather than asserting fake PIDs.
"""
import json
import os
from pathlib import Path
import sys
import uuid

sid, home, resume = sys.argv[1:]
store = Path(home) / '.fixture-sessions'
store.mkdir(parents=True, exist_ok=True)
path = store / (sid + '.json')
if resume == 'yes' and not path.exists():
    raise SystemExit(31)
data = json.loads(path.read_text()) if path.exists() else {'sid': sid, 'prompts': []}
path.write_text(json.dumps(data))
pending = None

def emit(value):
    print(json.dumps(value), flush=True)

def reply(text, *, interrupted=False):
    global pending
    if not interrupted:
        emit({'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': '[FIXTURE] ' + text}]})
    emit({'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': interrupted,
          'terminal_reason': 'aborted_tools' if interrupted else 'completed', 'num_turns': 1,
          'usage': {'input_tokens': 2, 'output_tokens': 3}})
    pending = None

emit({'type': 'system', 'subtype': 'init', 'data': {'session_id': sid}})
for line in sys.stdin:
    command = json.loads(line)
    if command['type'] == 'query':
        prompt = command['prompt']
        data['prompts'].append(prompt)
        path.write_text(json.dumps(data))
        pending = str(uuid.uuid4())
        if prompt == 'CRASH':
            os._exit(17)
        elif prompt == 'HANG':
            pass
        elif prompt.startswith('TOOL:'):
            name = prompt[5:]
            args = {'path': 'hello.txt'} if name == 'read_file' else {}
            emit({'fixture_tool': True, 'session_id': sid, 'call_id': pending, 'name': name, 'args': args})
        elif prompt == 'HISTORY':
            reply('|'.join(data['prompts']))
        else:
            reply(prompt)
    elif command['type'] == 'tool_result':
        if command['call_id'] == pending:
            reply(command['text'])
    elif command['type'] == 'interrupt':
        if not command.get('drop') and pending:
            reply('', interrupted=True)
    elif command['type'] == 'idle_event':
        emit({'type': 'TaskNotificationMessage', 'session_id': sid, 'status': 'completed', 'message': 'not public log content'})
