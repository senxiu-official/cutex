"""Request-body regression from the cross-review probe: no duplicated history."""
import asyncio
import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib
import json
import os
from pathlib import Path
import socket
import struct
import threading
import zlib

import pytest

from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import CONFLICTING_ENV
from cute_claude.subscription import SIDE_BOUNDARY_BLOCKS
from test_subscription import SubHarness

pytestmark = pytest.mark.skipif(not os.environ.get('CUTEX_TEST_CLAUDE_CLI'), reason='native')
PROMPT = 'PROBE-PROMPT-7731 reply briefly.'


def png_chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))


# The pure-input fixtures intentionally contain only a PNG-like header. This
# native probe needs a decodable image or the CLI silently omits it from the API.
PNG = base64.b64encode(b'\x89PNG\r\n\x1a\n'
    + png_chunk(b'IHDR', struct.pack('>IIBBBBB', 32, 32, 8, 2, 0, 0, 0))
    + png_chunk(b'IDAT', zlib.compress((b'\x00' + b'\xff\x00\x00' * 32) * 32))
    + png_chunk(b'IEND', b'')).decode()


def sse(number, text='fixture complete'):
    events = [
        ('message_start', {'type': 'message_start', 'message': {'id': f'msg_{number}', 'type': 'message',
            'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
            'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 0}}}),
        ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}),
        ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': text}}),
        ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
        ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn', 'stop_sequence': None}, 'usage': {'output_tokens': 2}}),
        ('message_stop', {'type': 'message_stop'})]
    return ''.join(f'event: {n}\ndata: {json.dumps(d)}\n\n' for n, d in events).encode()


@pytest.mark.parametrize('scenario', ['reset-dup', 'partial-text-reset', 'msgstart-reset', 'two-turns-dup',
                                     'reset-image-once', 'refresh-image-once', 'project-no-hooks', 'project-hooks'])
async def test_probe(tmp_path, monkeypatch, scenario):
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA'):
        p = tmp_path / name.lower(); p.mkdir(); monkeypatch.setenv(name, str(p))
    workspace = tmp_path / 'workspace'; workspace.mkdir()
    bodies, queries = [], []
    images = scenario in {'reset-image-once', 'refresh-image-once'}
    project = scenario in {'project-no-hooks', 'project-hooks'}
    if project:
        (workspace / '.claude').mkdir()
        settings = {'hooks': {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': 'echo configured-hook'}]}]}} if scenario == 'project-hooks' else {'permissions': {}}
        (workspace / '.claude/settings.json').write_text(json.dumps(settings))
    if scenario == 'refresh-image-once':
        monkeypatch.setenv('CUTE_CLAUDE_REFRESH_RETRY_DELAYS', '0.01')

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a): pass

        def reset(self):
            self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
            self.close_connection = True
            self.connection.close()

        def do_POST(self):
            self.connection.setblocking(True)
            raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            if not self.path.split('?')[0].endswith('/messages'):
                body = json.dumps({'input_tokens': 1}).encode()
                self.send_response(200); self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
                return
            try:
                bodies.append(json.loads(raw))
            except ValueError:
                bodies.append(None)
            n = len(bodies)
            first = n == 1 or (scenario == 'two-turns-dup' and n == 2)
            if first and scenario == 'refresh-image-once':
                raw = json.dumps({'type': 'error', 'error': {'type': 'api_error', 'message':
                    'Failed to refresh OAuth token: another Claude Code process is refreshing it or exited mid-refresh. '
                    'This is usually transient; retry in a minute'}}).encode()
                self.send_response(500); self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(raw))); self.end_headers(); self.wfile.write(raw)
                return
            if first and scenario in {'reset-dup', 'two-turns-dup', 'reset-image-once', 'project-no-hooks', 'project-hooks'} and not (scenario == 'two-turns-dup' and n == 1):
                self.reset(); return
            if first and scenario in {'partial-text-reset', 'msgstart-reset'}:
                full = sse(n, 'PARTIAL-VISIBLE-TEXT')
                cut = full.split(b'event: content_block_start')[0] if scenario == 'msgstart-reset' else full.split(b'event: content_block_stop')[0]
                self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(full) + 4096)); self.end_headers()
                self.wfile.write(cut); self.wfile.flush(); self.reset(); return
            data = sse(n)
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    th = threading.Thread(target=server.serve_forever, daemon=True); th.start()
    sdk = importlib.import_module('claude_agent_sdk')
    native = sdk.ClaudeSDKClient

    def client_for_fixture(*, options):
        options.env.update({'ANTHROPIC_API_KEY': 'synthetic-fixture-key',
                            'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{server.server_port}',
                            'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1',
                            'DISABLE_ERROR_REPORTING': '1'})
        c = native(options=options)
        oq = c.query

        async def q(*a, **k):
            queries.append(True)
            return await oq(*a, **k)
        c.query = q
        return c
    monkeypatch.setattr(sdk, 'ClaudeSDKClient', client_for_fixture)
    driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), tmp_path / 'native-config')
    h = SubHarness(Settings(workspace, tmp_path / 'state', model='claude-sonnet-4-6', mock=False,
                            backend='subscription', thinking='disabled', allow_exec=project,
                            setting_sources=('project',) if project else ()), driver)
    try:
        await h.init()
        tid = await h.thread()
        if images:
            monkeypatch.setattr(h.store, 'turn_note_frames', lambda turn_id: ['R47-PREPENDED-NOTE'])
            monkeypatch.setattr(h.engine.side_chats, 'boundary_pending', lambda thread_id: True)
            first = (await h.call('turn/start', {'threadId': tid, 'input': [
                {'type': 'text', 'text': PROMPT}, {'type': 'image', 'url': 'data:image/png;base64,' + PNG}]}))['turn']['id']
        else:
            first = await h.turn(tid, PROMPT)
        turns = [first]
        async with asyncio.timeout(90):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))
        if scenario == 'two-turns-dup':
            turns.append(await h.turn(tid, 'SECOND-PROMPT-4410'))
            async with asyncio.timeout(90):
                while h.engine.workers:
                    await asyncio.gather(*list(h.engine.workers.values()))
        summary = []
        for b in bodies:
            if not b:
                summary.append(None); continue
            msgs = b.get('messages', [])
            blocks = [block for msg in msgs for block in msg.get('content', []) if isinstance(block, dict)]
            dump = '\n'.join(block.get('text', '') for block in blocks if block.get('type') == 'text')
            summary.append({'nMessages': len(msgs), 'roles': [m.get('role') for m in msgs],
                            'promptCount': dump.count('PROBE-PROMPT-7731'),
                            'secondCount': dump.count('SECOND-PROMPT-4410'),
                            'partialCount': dump.count('PARTIAL-VISIBLE-TEXT'),
                            'imageCount': sum(block.get('type') == 'image' for block in blocks),
                            'noteCount': dump.count('R47-PREPENDED-NOTE'),
                            'sideBlockCounts': [dump.count(block) for block in SIDE_BOUNDARY_BLOCKS]})
        deltas = [e for e in h.out if e.get('method') == 'item/agentMessage/delta']
        visible_partial = any('PARTIAL' in json.dumps(e) for e in h.out)
        print(json.dumps({'scenario': scenario, 'http': len(bodies), 'queries': len(queries),
                          'status': [h.store.turn(t)['status'] for t in turns],
                          'errors': [h.store.turn(t).get('error') for t in turns],
                          'bodies': summary,
                          'deltaCount': len(deltas), 'partialEmitted': visible_partial}, indent=1))
        assert all(row['promptCount'] == 1 for row in summary)
        if scenario in {'partial-text-reset', 'msgstart-reset', 'project-hooks'}:
            assert len(queries) == 1
            assert h.store.turn(first)['status'] == 'failed'
        else:
            assert len(queries) == (3 if scenario == 'two-turns-dup' else 2)
            assert all(h.store.turn(turn)['status'] == 'completed' for turn in turns)
        if scenario == 'two-turns-dup':
            assert summary[-1]['secondCount'] == 1
        if images:
            assert all(row['imageCount'] == 1 and row['noteCount'] == 1 for row in summary)
            assert all(row['sideBlockCounts'] == [1] * len(SIDE_BOUNDARY_BLOCKS) for row in summary)
        if scenario == 'refresh-image-once':
            retries = [event['params'] for event in h.out if event.get('method') == 'error' and event['params'].get('willRetry')]
            assert len(retries) == 1 and 'login token refresh' in retries[0]['error']['message']
    finally:
        await h.close(); server.shutdown(); server.server_close(); th.join()
