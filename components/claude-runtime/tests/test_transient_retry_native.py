"""Real pinned CLI and SDK against loopback only, with synthetic child credentials.

The adapter runs its subscription policy (MAX_RETRIES=0, no ApiAccount). Only
the SDK client's child env is instrumented to target the fake API, never OAuth.
"""
import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib
import json
import os
from pathlib import Path
import socket
import struct
import threading

import pytest

from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import CONFLICTING_ENV
from test_subscription import SubHarness


pytestmark = pytest.mark.skipif(not os.environ.get('CUTEX_TEST_CLAUDE_CLI'), reason='explicit private native CLI required')


@pytest.mark.parametrize('scenario', ['reset-success', '500-success', '529-success', 'exhausted',
                                     '401', '400', '429', 'usage', 'tool-drop', 'tool-stream-drop', 'cancel'])
async def test_subscription_silent_retry_with_real_cli(tmp_path, monkeypatch, scenario):
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA'):
        path = tmp_path / name.lower()
        path.mkdir()
        monkeypatch.setenv(name, str(path))
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    readable = workspace / 'fixture.txt'
    readable.write_text('read this fixture only', encoding='utf-8')
    calls, queries, tool_calls = [], [], []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, status, body):
            raw = json.dumps(body).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(raw)))
            self.send_header('Retry-After', '0')
            self.end_headers()
            self.wfile.write(raw)

        def reset(self):
            self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
            self.close_connection = True
            self.connection.close()

        def do_POST(self):
            self.connection.setblocking(True)
            self.rfile.read(int(self.headers.get('Content-Length', 0)))
            if not self.path.split('?')[0].endswith('/messages'):
                self.reply(200, {'input_tokens': 1})
                return
            calls.append(self.path.split('?')[0])
            number = len(calls)
            if scenario == 'tool-stream-drop' and number > 1:
                # Begin a real SSE response, then truncate it after the earlier
                # model request already executed Read. No terminal event/result.
                raw = ('event: message_start\ndata: ' + json.dumps({'type': 'message_start',
                       'message': {'id': 'msg_truncated', 'type': 'message', 'role': 'assistant',
                                   'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
                                   'usage': {'input_tokens': 10, 'output_tokens': 0}}}) + '\n\n').encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(raw) + 4096))
                self.end_headers()
                self.wfile.write(raw)
                self.wfile.flush()
                self.reset()
                return
            if scenario == 'tool-drop' and number > 1:
                self.reset()
                return
            if scenario == 'reset-success' and number == 1:
                self.reset()
                return
            status = (500 if scenario in {'exhausted', 'cancel'} else
                      int(scenario[:3]) if scenario[:3].isdigit() and (number == 1 or '-' not in scenario) else
                      429 if scenario == 'usage' else None)
            if status:
                kind = {401: 'authentication_error', 400: 'invalid_request_error',
                        429: 'rate_limit_error', 529: 'overloaded_error'}.get(status, 'api_error')
                self.reply(status, {'type': 'error', 'error': {'type': kind,
                           'message': 'usage limit reached' if scenario == 'usage' else 'synthetic private error'}})
                return
            tool = scenario in {'tool-drop', 'tool-stream-drop'}
            block = {'type': 'tool_use', 'id': 'tool_once', 'name': 'Read', 'input': {}} if tool else {'type': 'text', 'text': ''}
            delta = ({'type': 'input_json_delta', 'partial_json': json.dumps({'file_path': str(readable)})}
                     if tool else {'type': 'text_delta', 'text': 'fixture complete'})
            events = [
                ('message_start', {'type': 'message_start', 'message': {'id': f'msg_{number}', 'type': 'message',
                    'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
                    'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 0}}}),
                ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': block}),
                ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': delta}),
                ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if tool else 'end_turn',
                    'stop_sequence': None}, 'usage': {'output_tokens': 2}}),
                ('message_stop', {'type': 'message_stop'})]
            raw = ''.join(f'event: {name}\ndata: {json.dumps(data)}\n\n' for name, data in events).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    sdk = importlib.import_module('claude_agent_sdk')
    native_client = sdk.ClaudeSDKClient

    def client_for_fixture(*, options):
        assert options.env['CLAUDE_CODE_MAX_RETRIES'] == '0'
        options.env.update({'ANTHROPIC_API_KEY': 'synthetic-fixture-key',
                            'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{server.server_port}',
                            'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1',
                            'DISABLE_ERROR_REPORTING': '1'})
        for matcher in options.hooks.get('PreToolUse', []):
            original = matcher.hooks[0]

            async def pre_tool(data, tool_use_id, context, original=original):
                tool_calls.append(data.get('tool_name'))
                return await original(data, tool_use_id, context)

            matcher.hooks[0] = pre_tool
        client = native_client(options=options)
        original_query = client.query

        async def query(*args, **kwargs):
            queries.append(True)
            return await original_query(*args, **kwargs)

        client.query = query
        return client

    monkeypatch.setattr(sdk, 'ClaudeSDKClient', client_for_fixture)
    driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), tmp_path / 'native-config')
    h = SubHarness(Settings(workspace, tmp_path / 'state', model='claude-sonnet-4-6', mock=False,
                            backend='subscription', thinking='disabled'), driver)
    try:
        await h.init()
        tid = await h.thread()
        turn = await h.turn(tid, 'Use the fixture Read tool.' if scenario in {'tool-drop', 'tool-stream-drop'} else 'Reply briefly.')
        if scenario == 'cancel':
            async with asyncio.timeout(45):
                while turn not in h.engine.retry_waiting:
                    if not h.engine.workers:
                        pytest.fail(f'no backoff: {h.store.turn(turn)}')
                    await asyncio.sleep(0.01)
            async with asyncio.timeout(2):
                await h.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
        async with asyncio.timeout(90):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))
        result = h.store.turn(turn)
        success = scenario.endswith('-success')
        expected_queries = 2 if success else 3 if scenario == 'exhausted' else 1
        evidence = {'scenario': scenario, 'httpAttempts': len(calls), 'queries': len(queries),
                    'tools': tool_calls, 'status': result['status'], 'error': result.get('error')}
        print(json.dumps(evidence))
        assert len(queries) == expected_queries, evidence
        assert result['status'] == ('completed' if success else 'interrupted' if scenario == 'cancel' else 'failed'), evidence
        # Pinned CLI 2.1.321 performs its own generic-400 recovery inside one
        # query even with MAX_RETRIES=0. The adapter must never add a query.
        if scenario == 'tool-stream-drop':
            assert len(calls) >= 2, evidence  # Native stream recovery remains inside the one query.
        else:
            assert len(calls) == (2 if scenario == 'tool-drop' else 4 if scenario == '400' else expected_queries), evidence
        if scenario == '400':
            assert result['error']['codexErrorInfo'] == 'badRequest', evidence
        assert tool_calls == (['Read'] if scenario in {'tool-drop', 'tool-stream-drop'} else []), evidence
        receipts = [e for e in h.out if e.get('method') == 'claude/input/contextPersisted']
        assert len(receipts) <= 1, evidence
        assert 'synthetic-fixture-key' not in json.dumps(evidence)
    finally:
        await h.close()
        server.shutdown()
        server.server_close()
        thread.join()
