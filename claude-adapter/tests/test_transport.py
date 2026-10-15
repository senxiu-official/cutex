from __future__ import annotations

import asyncio
import json
import os
import socket
import sys
from pathlib import Path

import pytest
from websockets.asyncio.client import connect
from websockets.exceptions import InvalidStatus

from cute_claude.client import Client
from cute_claude.transport import decode, rpc_token, websocket

TOKEN = 'test-token-at-least-twenty-four-chars'


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


async def ready(endpoint):
    for _ in range(100):
        try:
            return await Client.open(endpoint, TOKEN)
        except OSError:
            await asyncio.sleep(0.01)
    raise RuntimeError('listener not ready')


async def test_websocket_roundtrip_and_auth(h):
    endpoint = f'ws://127.0.0.1:{free_port()}'
    stop = asyncio.Event()
    task = asyncio.create_task(websocket(h.server, endpoint, TOKEN, stop))
    client = await ready(endpoint)
    try:
        with pytest.raises(InvalidStatus) as exc:
            await connect(endpoint)
        assert exc.value.response.status_code == 401
        with pytest.raises(InvalidStatus) as exc:
            await connect(endpoint, additional_headers={'Authorization': 'Bearer '+TOKEN}, origin='https://malicious.example')
        assert exc.value.response.status_code == 403
        tid = (await client.call('thread/start'))['thread']['id']
        turn_id = (await client.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'hello over WS'}]}))['turn']['id']
        completed = await client.wait_turn(turn_id)
        assert completed['status'] == 'completed'
        assert completed['items'][-1]['text'] == '[MOCK] hello over WS'
        await client.close()
        client = await Client.open(endpoint, TOKEN)
        snapshot = await client.call('thread/resume', {'threadId': tid})
        assert snapshot['thread']['turns'] == []
        page = await client.call('thread/turns/list', {'threadId': tid})
        assert page['data'][0]['id'] == turn_id
        assert len(h.provider.requests) == 1
    finally:
        await client.close()
        stop.set()
        await task


async def test_controller_discovers_os_allocated_port_without_credentials(h, tmp_path):
    ready_file = tmp_path / 'server.json'
    stop = asyncio.Event()
    task = asyncio.create_task(websocket(h.server, 'ws://127.0.0.1:0', TOKEN, stop, ready_file=ready_file))
    client = None
    try:
        async with asyncio.timeout(5):
            while not ready_file.exists():
                if task.done():
                    await task
                await asyncio.sleep(0.01)
        binding = json.loads(ready_file.read_text())
        assert binding['runtimeId'] == h.server.runtime_id
        assert binding['pid'] == os.getpid()
        assert not binding['endpoint'].endswith(':0')
        assert TOKEN not in ready_file.read_text()
        if os.name != 'nt':
            assert ready_file.stat().st_mode & 0o077 == 0
        client = await Client.open(binding['endpoint'], TOKEN)
        assert (await client.call('claude/capabilities'))['runtimeId'] == binding['runtimeId']
    finally:
        if client:
            await client.close()
        stop.set()
        await task


@pytest.mark.skipif(os.name == 'nt', reason='POSIX-only transport test')
async def test_unix_socket_is_websocket_not_raw_jsonl(h, tmp_path):
    private = tmp_path/'sockdir'
    private.mkdir(mode=0o700)
    path = private/'rpc.sock'
    endpoint = 'unix://' + str(path)
    stop = asyncio.Event()
    task = asyncio.create_task(websocket(h.server, endpoint, TOKEN, stop))
    client = await ready(endpoint)
    try:
        assert (path.stat().st_mode & 0o777) == 0o600
        assert (await client.call('claude/capabilities'))['backend'] == 'mock'
    finally:
        await client.close()
        stop.set()
        await task
    assert not path.exists()


async def test_stdio_subprocess_clean_jsonl(tmp_path):
    workspace = tmp_path/'workspace'
    workspace.mkdir()
    env = {**os.environ, 'PYTHONPATH': str(Path(__file__).parents[1]/'src')}
    proc = await asyncio.create_subprocess_exec(sys.executable, '-m', 'cute_claude', '--mock', '--workspace', str(workspace), '--state-dir', str(tmp_path/'state'), env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    async def send(message):
        proc.stdin.write(json.dumps(message).encode()+b'\n')
        await proc.stdin.drain()
    async def read():
        raw = await asyncio.wait_for(proc.stdout.readline(), 5)
        assert raw, (await proc.stderr.read()).decode()
        return json.loads(raw)
    try:
        await send({'id': 1, 'method': 'initialize', 'params': {}})
        assert (await read())['id'] == 1
        await send({'method': 'initialized'})
        await send({'id': 2, 'method': 'thread/start', 'params': {}})
        tid = (await read())['result']['thread']['id']
        await send({'id': 3, 'method': 'turn/start', 'params': {'threadId': tid, 'input': [{'type': 'text', 'text': 'hello over stdio'}]}})
        completed = None
        async with asyncio.timeout(5):
            while not completed:
                message = await read()
                if message.get('method') == 'turn/completed':
                    completed = message['params']['turn']
        assert completed['status'] == 'completed'
        proc.stdin.close()
        await asyncio.wait_for(proc.wait(), 5)
        assert proc.returncode == 0
        assert 'provider=MOCK' in (await proc.stderr.read()).decode()
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()


def test_rpc_token_private_and_stable(tmp_path, monkeypatch):
    monkeypatch.delenv('DEMO_RPC_TOKEN_TEST', raising=False)
    token = rpc_token(tmp_path, 'DEMO_RPC_TOKEN_TEST')
    assert token == rpc_token(tmp_path, 'DEMO_RPC_TOKEN_TEST')
    assert len(token) >= 24
    if os.name != 'nt':
        assert (tmp_path/'rpc-token').stat().st_mode & 0o077 == 0


def test_transport_rejects_non_json_constants():
    for raw in ['{"value":NaN}', '{"value":Infinity}', '{broken']:
        with pytest.raises(ValueError):
            decode(raw)


async def test_resume_history_larger_than_request_limit_is_complete(h):
    tid = await h.thread()
    turn = await h.turn(tid, 'fixture')
    await h.drain()
    large = '中文 history ' * 110000
    h.store.put_item(turn, {'id':'large-history','type':'agentMessage','text':large})
    endpoint = f'ws://127.0.0.1:{free_port()}'
    stop = asyncio.Event()
    task = asyncio.create_task(websocket(h.server, endpoint, TOKEN, stop))
    client = await ready(endpoint)
    try:
        snapshot = await client.call('thread/resume', {'threadId':tid})
        assert snapshot['thread']['turns'] == []
        page = await client.call('thread/items/list', {'threadId':tid, 'sortDirection':'desc'})
        items = [entry['item'] for entry in page['data']]
        assert next(i for i in items if i['id']=='large-history')['text'] == large
        assert len(json.dumps(page).encode()) > 1_000_000
        assert len(json.dumps(snapshot).encode()) < 10000
        assert len(h.provider.requests) == 1
    finally:
        await client.close();stop.set();await task

async def test_outbox_byte_budget_bounds_large_replies(monkeypatch):
    import cute_claude.transport as transport
    from cute_claude.errors import RpcError
    monkeypatch.setattr(transport, 'MAX_RESPONSE', 100)
    monkeypatch.setattr(transport, 'MAX_OUTBOX_BYTES', 120)
    block = asyncio.Event()
    async def writer(raw): await block.wait()
    box = transport.Outbox(writer)
    try:
        with pytest.raises(RpcError): await box.send({'text':'x'*101})
        await box.send({'text':'x'*70})
        with pytest.raises(ConnectionError): await box.send({'text':'y'*70})
        assert box.pending_bytes <= 120
    finally:
        block.set();await box.close()
