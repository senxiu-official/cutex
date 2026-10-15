#!/usr/bin/env python3
"""Real local HTTP job tool + real WS inbound callback, using an offline mock model.

This is an example bridge contract, NOT the production CUTEX Job Service API.
"""
from __future__ import annotations

import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import socket
import sys
import tempfile
import threading
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT/'src'))
from cute_claude.client import Client
from cute_claude.config import Settings
from cute_claude.engine import Engine
from cute_claude.protocol import Server
from cute_claude.provider import MockProvider
from cute_claude.store import Store
from cute_claude.tools import ToolRegistry
from cute_claude.transport import websocket


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


async def demo():
    with tempfile.TemporaryDirectory(prefix='cute-claude-job-demo-') as raw:
        base = Path(raw)
        work = base/'workspace'
        work.mkdir()
        token = secrets.token_urlsafe(32)
        job_token = secrets.token_urlsafe(32)
        endpoint = f'ws://127.0.0.1:{port()}'
        loop = asyncio.get_running_loop()
        callbacks: list[asyncio.Future] = []
        seen: dict[str, str] = {}
        lock = threading.Lock()

        async def complete_job(job_id, payload):
            await asyncio.sleep(0.05)
            value = payload['arguments']['a'] + payload['arguments']['b']
            controller = await Client.open(endpoint, token)
            try:
                return await controller.call('claude/input/submit', {
                    'threadId': payload['context']['threadId'], 'messageId': f'job:{job_id}:completed',
                    'source': {'kind': 'service', 'id': 'demo-job-service'}, 'eventType': 'job.completed',
                    'text': json.dumps({'jobId': job_id, 'status': 'completed', 'result': value})})
            finally:
                await controller.close()

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                if self.path != '/tool' or self.headers.get('Authorization') != 'Bearer '+job_token:
                    self.send_error(403)
                    return
                length = int(self.headers.get('Content-Length', '0'))
                if not 0 < length <= 65536:
                    self.send_error(413)
                    return
                payload = json.loads(self.rfile.read(length))
                key = payload['context']['callId']
                with lock:
                    duplicate = key in seen
                    job_id = seen.setdefault(key, 'job-'+str(uuid.uuid4()))
                if not duplicate:
                    future = asyncio.run_coroutine_threadsafe(complete_job(job_id, payload), loop)
                    callbacks.append(future)
                result = json.dumps({'jobId': job_id, 'state': 'running'}).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(result)))
                self.end_headers()
                self.wfile.write(result)

        jobs = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        job_thread = threading.Thread(target=jobs.serve_forever, daemon=True)
        job_thread.start()
        config = base/'http-tools.json'
        config.write_text(json.dumps([{'name': 'job_submit', 'description': 'Submit an asynchronous addition job; completion arrives later as external input.',
            'input_schema': {'type': 'object', 'properties': {'a': {'type': 'integer'}, 'b': {'type': 'integer'}}, 'required': ['a','b'], 'additionalProperties': False},
            'url': f'http://127.0.0.1:{jobs.server_port}/tool', 'token_env': 'CUTE_CLAUDE_DEMO_JOB_TOKEN'}]))
        old_token = os.environ.get('CUTE_CLAUDE_DEMO_JOB_TOKEN')
        os.environ['CUTE_CLAUDE_DEMO_JOB_TOKEN'] = job_token
        settings = Settings(work, base/'state')
        store = Store(settings.state_dir)
        tools = ToolRegistry(settings)
        tools.load_http_tools(config)
        provider = MockProvider()
        engine = Engine(settings, store, provider, tools)
        server = Server(settings, store, engine)
        stop = asyncio.Event()
        serving = asyncio.create_task(websocket(server, endpoint, token, stop))
        client = None
        try:
            for _ in range(100):
                try:
                    client = await Client.open(endpoint, token)
                    break
                except OSError:
                    await asyncio.sleep(0.01)
            assert client is not None, 'listener did not start'
            tid = (await client.call('thread/start'))['thread']['id']
            turn_id = (await client.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'tool:job_submit {"a":20,"b":22}'}]}))['turn']['id']
            await client.wait_turn(turn_id)
            print('[OK] Model tool call -> ordinary HTTP -> async job accepted')
            async with asyncio.timeout(10):
                while not callbacks:
                    await asyncio.sleep(0.01)
                receipt = await asyncio.wrap_future(callbacks[0])
            turn = await client.wait_turn(receipt['turnId'])
            assert turn['status'] == 'completed'
            assert '42' in turn['items'][-1]['text']
            assert len(seen) == 1 and len(provider.requests) == 3
            print('[OK] Job computes 20 + 22 = 42')
            print('[OK] Service -> WS generic inbound -> separate model turn -> response contains 42')
            print('[OK] One job; three MOCK inference requests; no MCP and no Claude Code CLI')
            print('HTTP job completion demo passed. Production Job Service wiring is still an integration task.')
        finally:
            if client:
                await client.close()
            stop.set()
            await serving
            await engine.close()
            store.close()
            await asyncio.to_thread(jobs.shutdown)
            jobs.server_close()
            job_thread.join(timeout=2)
            if old_token is None:
                os.environ.pop('CUTE_CLAUDE_DEMO_JOB_TOKEN', None)
            else:
                os.environ['CUTE_CLAUDE_DEMO_JOB_TOKEN'] = old_token


if __name__ == '__main__':
    asyncio.run(demo())
