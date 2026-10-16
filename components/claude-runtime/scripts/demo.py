#!/usr/bin/env python3
"""Offline subprocess demo. No account, API key, Claude CLI, or cute-codex required."""
from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


async def demo(record: Path | None) -> None:
    trace, completed = [], {}
    with tempfile.TemporaryDirectory(prefix='cute-claude-demo-') as temporary:
        base = Path(temporary)
        workspace = base/'workspace'
        workspace.mkdir()
        (workspace/'hello.txt').write_text('This file was really read by the Python tool.\n')
        env = {**os.environ, 'PYTHONPATH': str(ROOT/'src') + os.pathsep + os.environ.get('PYTHONPATH', '')}
        process = await asyncio.create_subprocess_exec(sys.executable, '-m', 'cute_claude', '--mock', '--workspace', str(workspace), '--state-dir', str(base/'state'),
                env=env, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        counter = 0
        async def write(message):
            trace.append({'direction': 'client->server', 'message': message})
            process.stdin.write((json.dumps(message)+'\n').encode())
            await process.stdin.drain()
        async def read():
            raw = await asyncio.wait_for(process.stdout.readline(), 10)
            if not raw:
                raise RuntimeError((await process.stderr.read()).decode())
            message = json.loads(raw)
            trace.append({'direction': 'server->client', 'message': message})
            if message.get('method') == 'turn/completed':
                completed[message['params']['turn']['id']] = message['params']['turn']
            return message
        async def call(method, params=None):
            nonlocal counter
            counter += 1
            await write({'id': counter, 'method': method, 'params': params or {}})
            while True:
                message = await read()
                if message.get('id') == counter:
                    if 'error' in message:
                        raise RuntimeError(message['error'])
                    return message['result']
        async def wait(turn_id):
            while turn_id not in completed:
                await read()
            assert completed[turn_id]['status'] == 'completed', completed[turn_id]
        try:
            await call('initialize', {'clientInfo': {'name': 'offline-demo', 'version': '0.1.0'}, 'capabilities': {'experimentalApi': True, 'claudeExtensions': True}})
            await write({'method': 'initialized'})
            await call('model/list')
            tid = (await call('thread/start'))['thread']['id']
            print('[OK] JSONL subprocess handshake and persistent thread')
            turn_id = (await call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'tool:read_file {"path":"hello.txt"}'}]}))['turn']['id']
            await wait(turn_id)
            assert 'really read' in completed[turn_id]['items'][-1]['text']
            print('[OK] Streaming mock model -> real read_file -> tool result -> final response')
            payload = {'threadId': tid, 'messageId': 'director-message-001', 'source': {'kind': 'agent', 'id': 'director'}, 'eventType': 'task.message', 'text': 'Confirm this external agent message arrived.'}
            first = await call('claude/input/submit', payload)
            duplicate = await call('claude/input/submit', payload)
            assert duplicate['duplicate'] and duplicate['turnId'] == first['turnId']
            await wait(first['turnId'])
            print('[OK] Generic inbound preserves source and deduplicates messageId')
            snapshot = await call('thread/resume', {'threadId': tid})
            assert snapshot['thread']['turns'] == []
            page = await call('thread/turns/list', {'threadId': tid})
            assert len(page['data']) == 2
            print('[OK] Resume and pagination return both turns without creating a model request')
            info = await call('claude/thread/inspect', {'threadId': tid})
            requests = sum(len(u['modelRequests']) for u in info['usage'])
            assert requests == 3
            print(f'[OK] Exactly {requests} MOCK inference requests; no hidden retry or background model')
        finally:
            process.stdin.close()
            try:
                await asyncio.wait_for(process.wait(), 5)
            except TimeoutError:
                process.kill()
                await process.wait()
            if process.returncode:
                raise RuntimeError((await process.stderr.read()).decode())
    if record:
        record.parent.mkdir(parents=True, exist_ok=True)
        record.write_text(''.join(json.dumps(row, ensure_ascii=False)+'\n' for row in trace), encoding='utf-8')
        print(f'[OK] Protocol trace: {record}')
    print('Offline demo passed. This was not a live Claude model run.')


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--record', type=Path)
    asyncio.run(demo(p.parse_args().record))
