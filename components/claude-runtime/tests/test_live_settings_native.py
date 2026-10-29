"""Opt-in native check: model and effort changed while a turn runs reach the next model request of that
same turn, without a new Claude Code child (claude-r24, live_settings).

Set CUTEX_TEST_CLAUDE_CLI to an explicitly selected Claude executable. A fake loopback Anthropic endpoint
answers the turn's first request with one Bash `sleep` tool call and records every request; only a synthetic
key is used, no real credential, login or network.
"""
import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import threading

import pytest

from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from test_api_key_tool_env import message
from test_subscription import SubHarness

FAKE_KEY = 'sk-ant-fixture-0000-do-not-use-live-settings-probe'


@pytest.mark.skipif(not os.environ.get('CUTEX_TEST_CLAUDE_CLI'), reason='private native CLI not explicitly selected')
async def test_mid_turn_model_and_effort_change_applies_to_the_next_step(tmp_path, monkeypatch):
    requests, tool_started = [], threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args): pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            if self.path.split('?')[0].endswith('/messages/count_tokens'):
                return self.reply(json.dumps({'input_tokens': 1}).encode(), 'application/json')
            body = json.loads(raw or b'{}')
            if not body.get('stream'):
                # Claude Code's `set_model` validates the model with one small non-streaming request.
                return self.reply(json.dumps({'id': 'msg_check', 'type': 'message', 'role': 'assistant', 'model': body.get('model'),
                    'content': [{'type': 'text', 'text': 'ok'}], 'stop_reason': 'end_turn', 'stop_sequence': None,
                    'usage': {'input_tokens': 1, 'output_tokens': 1}}).encode(), 'application/json')
            user = next((m for m in reversed(body.get('messages') or []) if m.get('role') == 'user'), {})
            content = user.get('content') if isinstance(user.get('content'), list) else []
            result = any(isinstance(c, dict) and c.get('type') == 'tool_result' for c in content)
            requests.append({'model': body.get('model'), 'effort': (body.get('output_config') or {}).get('effort'),
                             'after_tool': result})
            if not result and not tool_started.is_set():
                tool_started.set()
                out = message([{'type': 'tool_use', 'id': 'toolu_fixture_sleep', 'name': 'Bash',
                                'input': {'command': 'sleep 4', 'description': 'wait'}}], 'tool_use')
            else:
                out = message([{'type': 'text', 'text': 'done'}], 'end_turn')
            self.reply(out, 'text/event-stream')

        def reply(self, raw, ctype):
            self.send_response(200)
            self.send_header('Content-Type', ctype)
            self.send_header('Content-Length', str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    serving = threading.Thread(target=server.serve_forever, daemon=True)
    serving.start()
    private_home = tmp_path / 'home'
    private_home.mkdir()
    monkeypatch.setenv('HOME', str(private_home))
    monkeypatch.setenv('USERPROFILE', str(private_home))
    for name in CONFLICTING_ENV + ('CLAUDE_CODE_EFFORT_LEVEL',):
        monkeypatch.delenv(name, raising=False)
    key_file = tmp_path / 'keys' / 'ca-loopback.json'
    key_file.parent.mkdir()
    key_file.write_text(json.dumps({'apiKey': FAKE_KEY}), encoding='utf-8')
    account = ApiAccount('ca-loopback', 'ANTHROPIC_API_KEY', f'http://127.0.0.1:{server.server_port}', {
        'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1',
    }, FAKE_KEY, key_file)
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), private_home / 'claude', api_account=account)
    h = SubHarness(Settings(workspace, tmp_path / 'state', model='claude-opus-5-5', effort='high', mock=False,
                            backend='subscription', allow_exec=True, allow_write=True), driver)
    try:
        await h.init()
        tid = await h.thread()
        turn = await h.turn(tid, 'Run the fixture tool.')
        assert await asyncio.to_thread(tool_started.wait, 60), 'the first step asked for the tool'
        owner = h.engine.driver.owners[tid]
        generation = owner.native_connection_generation
        await asyncio.sleep(0.5)  # the tool is running now
        await h.call('thread/settings/update', {'threadId': tid, 'model': 'claude-sonnet-5-5', 'effort': 'low'})
        async with asyncio.timeout(120):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))
        result = h.store.turn(turn)
        assert result['status'] == 'completed', result.get('error')
        assert owner.native_connection_generation == generation, 'no new Claude Code child'
        first, after = requests[0], next(r for r in requests if r['after_tool'])
        assert (first['model'], first['effort']) == ('claude-opus-5-5', 'high')
        assert (after['model'], after['effort']) == ('claude-sonnet-5-5', 'low'), requests
    finally:
        await h.close()
        server.shutdown()
        server.server_close()
        serving.join()
