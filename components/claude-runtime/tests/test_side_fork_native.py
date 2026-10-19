"""Opt-in real SDK/CLI probe of Side chats and busy forks against a loopback model; never real credentials.

CUTEX_SIDE_FORK_NATIVE_SMOKE=1 python -m pytest -q -s tests/test_side_fork_native.py
"""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from test_subscription import SubHarness

pytestmark = pytest.mark.skipif(os.environ.get('CUTEX_SIDE_FORK_NATIVE_SMOKE') != '1', reason='opt-in local SDK/CLI probe')


async def test_real_cli_side_chat_and_busy_fork(tmp_path, monkeypatch):
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA'):
        isolated = tmp_path / name.lower(); isolated.mkdir()
        monkeypatch.setenv(name, str(isolated))
    work = tmp_path / 'workspace'; work.mkdir()
    requests, held, release = [], threading.Event(), threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            data = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
            if '/messages' not in self.path or '/count_tokens' in self.path:
                body = json.dumps({'input_tokens': 1}).encode()
                self.send_response(200); self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
                return
            requests.append(data)
            if 'HOLD_PARENT' in json.dumps(data['messages'][-1]):
                held.set()
                if not release.wait(45):
                    self.send_error(503); return
            events = [
                ('message_start', {'type': 'message_start', 'message': {'id': f'msg_{len(requests)}', 'type': 'message',
                    'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
                    'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 0}}}),
                ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}),
                ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                         'delta': {'type': 'text_delta', 'text': f'answer {len(requests)}'}}),
                ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn', 'stop_sequence': None},
                                   'usage': {'output_tokens': 5}}),
                ('message_stop', {'type': 'message_stop'}),
            ]
            body = ''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name, value in events).encode()
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    base = f'http://127.0.0.1:{server.server_address[1]}'
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    bundled = Path(importlib.util.find_spec('claude_agent_sdk').origin).parent / '_bundled'
    cli = bundled / ('claude.exe' if os.name == 'nt' else 'claude')
    key_file = tmp_path / 'keys' / 'loopback.json'
    key_file.parent.mkdir()
    key_file.write_text(json.dumps({'apiKey': 'local-fixture-not-a-real-key'}), encoding='utf-8')
    account = ApiAccount('loopback-fixture', 'ANTHROPIC_API_KEY', base,
        {'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1'},
        'local-fixture-not-a-real-key', key_file)
    config = tmp_path / 'native-config'
    driver = OfficialSdkDriver(cli, config, api_account=account)
    h = SubHarness(Settings(work, tmp_path / 'adapter', model='claude-sonnet-4-6', backend='subscription', mock=False,
                            thinking='disabled'), driver)

    def transcript(sid):
        return next(config.glob(f'projects/*/{sid}.jsonl')).read_text(encoding='utf-8')

    def conversation(request):
        return json.dumps(request['messages'])

    async def drain():
        async with asyncio.timeout(60):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))

    try:
        await h.init()
        tid = await h.thread()
        first = await h.turn(tid, 'Remember PARENT_MARKER_11.')
        await drain()
        assert h.store.turn(first, include_items=False)['status'] == 'completed'
        parent_sid = h.engine.session(tid)['session_id']

        # Side: sees the parent's context, writes nothing into it.
        chat = (await h.call('claude/side/open', {'threadId': tid}))['chat']
        assert chat['forkedThroughTurnId'] == first
        await h.call('claude/side/send', {'threadId': tid, 'chatId': chat['id'], 'text': 'SIDE_QUESTION_23', 'clientMessageId': 'q'})
        async with asyncio.timeout(60):
            while (chat := (await h.call('claude/side/read', {'threadId': tid, 'chatId': chat['id'],
                                                              'afterRevision': chat['revision'], 'waitMs': 1000}))['chat'])['status'] != 'ready':
                pass
        side_request = next(r for r in requests if 'SIDE_QUESTION_23' in conversation(r))
        assert 'PARENT_MARKER_11' in conversation(side_request)
        assert 'Side conversation boundary.' in conversation(side_request)
        assert 'You are in a side conversation' in conversation(side_request)
        parent_request = next(r for r in requests if 'PARENT_MARKER_11' in conversation(r))
        assert side_request['system'] == parent_request['system']  # Same prompt-cache prefix as the parent.
        assert any(e['method'] == 'item/completed' and e['params']['item']['type'] == 'agentMessage' for e in chat['events'])
        assert 'SIDE_QUESTION_23' not in transcript(parent_sid)
        side_sid = h.engine.session(chat['threadId'])['session_id']
        assert 'SIDE_QUESTION_23' in transcript(side_sid)
        await h.call('claude/side/close', {'threadId': tid, 'chatId': chat['id']})
        assert not list(config.glob(f'projects/*/{side_sid}.jsonl'))  # Discarded with the chat.

        # Busy fork: the running turn is left out, the parent keeps running.
        running = await h.turn(tid, 'HOLD_PARENT now.')
        assert await asyncio.to_thread(held.wait, 45), 'CLI never reached the loopback provider'
        fork = await h.server.r8.sessions.fork(tid, {})
        assert fork['forkedThroughTurnId'] == first and fork['sourceRuntimeUntouched'] is True
        assert h.store.turn(running, include_items=False)['status'] == 'inProgress'
        forked = transcript(fork['nativeSessionId'])
        assert 'PARENT_MARKER_11' in forked and 'HOLD_PARENT' not in forked
        release.set()
        await drain()
        assert h.store.turn(running, include_items=False)['status'] == 'completed'
        question = await h.turn(fork['threadId'], 'FORK_QUESTION_31')
        await drain()
        assert h.store.turn(question, include_items=False)['status'] == 'completed'
        fork_request = next(r for r in reversed(requests) if 'FORK_QUESTION_31' in conversation(r))
        assert 'PARENT_MARKER_11' in conversation(fork_request) and 'HOLD_PARENT' not in conversation(fork_request)
        print(json.dumps({'probe': 'real-sdk-cli-loopback', 'kind': 'side-and-busy-fork', 'modelRequests': len(requests),
                          'sideSawParent': True, 'parentUntouched': True, 'busyForkLeftRunningTurnOut': True}))
    finally:
        release.set()
        await h.close()
        server.shutdown(); server.server_close(); thread.join(timeout=2)
