"""Opt-in real SDK/CLI probe against a loopback model; never real credentials.

CUTEX_BUSY_DELIVERY_NATIVE_SMOKE=1 python -m pytest -q -s tests/test_busy_delivery_native.py
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
from cute_claude.external_input import semantic_digest
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from test_subscription import SubHarness

pytestmark = pytest.mark.skipif(os.environ.get('CUTEX_BUSY_DELIVERY_NATIVE_SMOKE') != '1', reason='opt-in local SDK/CLI probe')


@pytest.mark.parametrize('kind,tools', [('wake', True), ('wake', False), ('result', True), ('result', False)])
async def test_real_cli_consumes_hook_or_falls_back_once(tmp_path, monkeypatch, kind, tools):
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA'):
        isolated = tmp_path / name.lower(); isolated.mkdir()
        monkeypatch.setenv(name, str(isolated))
    work = tmp_path / 'workspace'; work.mkdir()
    readable = work / 'fixture.txt'; readable.write_text('local fixture')
    first, release = threading.Event(), threading.Event()
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            data = json.loads(raw)
            if '/messages' not in self.path or '/count_tokens' in self.path:
                body = json.dumps({'input_tokens': 1}).encode()
                self.send_response(200); self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)
                return
            requests.append(data)
            number = len(requests)
            if number == 1:
                first.set()
                if not release.wait(45):
                    self.send_error(503); return
            use_tool = tools and number == 1
            block = ({'type': 'tool_use', 'id': 'call_fixture', 'name': 'Read', 'input': {}}
                     if use_tool else {'type': 'text', 'text': ''})
            delta = ({'type': 'input_json_delta', 'partial_json': json.dumps({'file_path': str(readable)})}
                     if use_tool else {'type': 'text_delta', 'text': 'fixture complete'})
            events = [
                ('message_start', {'type': 'message_start', 'message': {'id': f'msg_{number}', 'type': 'message',
                    'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
                    'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 0}}}),
                ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': block}),
                ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': delta}),
                ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if use_tool else 'end_turn',
                    'stop_sequence': None}, 'usage': {'output_tokens': 5}}),
                ('message_stop', {'type': 'message_stop'}),
            ]
            body = ''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name, value in events).encode()
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 25970), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    spec = importlib.util.find_spec('claude_agent_sdk')
    cli = Path(spec.origin).parent / '_bundled' / 'claude.exe'
    account = ApiAccount('loopback-fixture', 'ANTHROPIC_API_KEY', 'http://127.0.0.1:25970',
        {'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1'},
        'local-fixture-not-a-real-key')
    driver = OfficialSdkDriver(cli, tmp_path / 'native-config', api_account=account)
    h = SubHarness(Settings(work, tmp_path / 'adapter', model='claude-sonnet-4-6', backend='subscription', mock=False,
                            thinking='disabled', external_owner_id='owner', external_runtime_generation=7), driver)
    try:
        await h.init()
        tid = await h.thread()
        turn = await h.turn(tid, 'Run the local fixture.')
        assert await asyncio.to_thread(first.wait, 45), 'CLI never reached the loopback provider'
        value = {'version': 1, 'ownerId': 'owner', 'threadId': tid, 'runtimeGeneration': 7,
                 'message': {'id': 'probe-wake', 'source': {'kind': 'agent', 'id': 'peer'},
                             'type': 'agent_message', 'delivery': 'soon', 'text': 'BUSY_WAKE_MARKER_07'}}
        value['semanticSha256'] = semantic_digest(value)
        if kind == 'wake':
            accepted = await h.call('thread/externalInput/submit', value)
            assert accepted['statuses'][0]['deliveryState'] == 'pending'
        else:
            accepted = await h.call('claude/input/submit', {
                'threadId': tid, 'messageId': 'cutex-ask-soon-probe-result', 'source': {'kind': 'service', 'id': 'cutex-asks'},
                'eventType': 'ANSWER', 'text': 'BUSY_WAKE_MARKER_07', 'admission': 'next_step',
                'metadata': {'deliveryMode': 'soon', 'asks': ['ask-fixture']}})
            assert accepted['contextPersisted'] is False
        release.set()
        async with asyncio.timeout(60):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))
        if kind == 'wake':
            receipt = h.server.external_inputs._status(tid, 'probe-wake', value['semanticSha256'])
            assert receipt['deliveryState'] == 'context_persisted'
            consumed_turn = receipt['receipt']['turnId']
        else:
            receipt = await h.call('claude/input/status', {'threadId': tid, 'messageId': 'cutex-ask-soon-probe-result'})
            assert receipt['contextPersisted'] and receipt['ask'] == {'asks': ['ask-fixture']}
            consumed_turn = receipt['contextTurnId']
        turns = h.store.db.execute('SELECT id,state FROM turns ORDER BY rowid').fetchall()
        assert all(row['state'] == 'completed' for row in turns), [dict(row) for row in turns]
        assert len(turns) == (1 if tools else 2)
        assert len(requests) == 2 and 'BUSY_WAKE_MARKER_07' in json.dumps(requests[1]['messages'])
        assert (consumed_turn == turn) == tools
        print(json.dumps({'probe': 'real-sdk-cli-loopback', 'kind': kind, 'tools': tools, 'modelRequests': len(requests),
                          'turns': len(turns), 'contextPersisted': True, 'sameTurn': tools}))
    finally:
        release.set()
        await h.close()
        server.shutdown(); server.server_close(); thread.join(timeout=2)


async def test_real_cli_owner_wake_reads_notes_in_its_first_request(tmp_path, monkeypatch):
    """A pending note is in an owner turn's first model request (owner's 唤醒,
    hachimi 2026-10-19), even without tool calls; Claude Code expands the owner's
    @path mention but not one in the note (an earlier text block)."""
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA'):
        isolated = tmp_path / name.lower(); isolated.mkdir()
        monkeypatch.setenv(name, str(isolated))
    work = tmp_path / 'workspace'; work.mkdir()
    (work / 'owner.txt').write_text('OWNER_FILE_CONTENT_31')
    (work / 'secret.txt').write_text('NOTE_FILE_CONTENT_47')
    requests = []

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
            events = [
                ('message_start', {'type': 'message_start', 'message': {'id': f'msg_{len(requests)}', 'type': 'message',
                    'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None,
                    'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 0}}}),
                ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}),
                ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': 'done'}}),
                ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn', 'stop_sequence': None},
                                   'usage': {'output_tokens': 5}}),
                ('message_stop', {'type': 'message_stop'}),
            ]
            body = ''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name, value in events).encode()
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 25970), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    bundled = Path(importlib.util.find_spec('claude_agent_sdk').origin).parent / '_bundled'
    cli = bundled / ('claude.exe' if os.name == 'nt' else 'claude')
    key_file = tmp_path / 'keys' / 'loopback.json'  # the key helper reads it
    key_file.parent.mkdir()
    key_file.write_text(json.dumps({'apiKey': 'local-fixture-not-a-real-key'}), encoding='utf-8')
    account = ApiAccount('loopback-fixture', 'ANTHROPIC_API_KEY', 'http://127.0.0.1:25970',
        {'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1'},
        'local-fixture-not-a-real-key', key_file)
    driver = OfficialSdkDriver(cli, tmp_path / 'native-config', api_account=account)
    h = SubHarness(Settings(work, tmp_path / 'adapter', model='claude-sonnet-4-6', backend='subscription', mock=False,
                            thinking='disabled'), driver)
    try:
        await h.init()
        tid = await h.thread()
        await h.call('claude/input/submit', {'threadId': tid, 'messageId': 'probe-note', 'source': {'kind': 'agent', 'id': 'peer'},
            'eventType': 'agent_message', 'text': 'NOTE_MARKER_83 see @secret.txt',
            'metadata': {'sourceName': 'peer', 'deliveryMode': 'passive'}})
        turn = await h.turn(tid, 'Read and handle the unread notes; also @owner.txt')
        async with asyncio.timeout(60):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))
        assert h.store.turn(turn, include_items=False)['status'] == 'completed', h.store.turn(turn, include_items=False)['error']
        assert len(requests) == 1, 'no tool call: one model request'
        first = json.dumps(requests[0]['messages'])
        assert 'NOTE_MARKER_83' in first, 'the note is in the first request'
        assert 'OWNER_FILE_CONTENT_31' in first, "the owner's @path mention expands"
        assert 'NOTE_FILE_CONTENT_47' not in first, "the note's @path mention does not"
        status = await h.call('claude/input/status', {'threadId': tid, 'messageId': 'probe-note'})
        assert status['state'] == 'injected'
        print(json.dumps({'probe': 'real-sdk-cli-loopback', 'kind': 'owner-turn-notes', 'modelRequests': len(requests),
                          'noteInFirstRequest': True, 'ownerMentionExpanded': True, 'noteMentionExpanded': False}))
    finally:
        await h.close()
        server.shutdown(); server.server_close(); thread.join(timeout=2)
