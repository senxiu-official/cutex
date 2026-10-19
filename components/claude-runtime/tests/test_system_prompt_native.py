"""Opt-in real SDK/CLI probe of a resumed session's system prompt against a loopback model; never real credentials.

CUTEX_SYSTEM_PROMPT_NATIVE_SMOKE=1 python -m pytest -q -s tests/test_system_prompt_native.py

Claude Code records a session's system prompt on its first request and sends
that record on every resume even when `--append-system-prompt` changed
(`systemPromptSnapshot`, default on), until a compaction. The adapter turns the
snapshot off for a resume whose prompt changed (system_prompt_option).
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
from cute_claude.sdk_runtime import PROMPT_RECORD, prompt_digest
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from test_subscription import SubHarness

pytestmark = pytest.mark.skipif(os.environ.get('CUTEX_SYSTEM_PROMPT_NATIVE_SMOKE') != '1', reason='opt-in local SDK/CLI probe')

MARKERS = ('ROLE_ALPHA_17', 'ROLE_BETA_29')


async def test_real_cli_resume_sends_the_changed_system_prompt(tmp_path, monkeypatch):
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA'):
        isolated = tmp_path / name.lower(); isolated.mkdir()
        monkeypatch.setenv(name, str(isolated))
    work = tmp_path / 'workspace'; work.mkdir()
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
    driver = OfficialSdkDriver(cli, tmp_path / 'native-config', api_account=account)
    h = SubHarness(Settings(work, tmp_path / 'adapter', model='claude-sonnet-4-6', backend='subscription', mock=False,
                            thinking='disabled'), driver)

    async def drain():
        async with asyncio.timeout(60):
            while h.engine.workers:
                await asyncio.gather(*list(h.engine.workers.values()))

    async def ask(tid, label, instructions=None):
        """One turn in a fresh Claude Code process (a resume after the first); the markers its system prompt had."""
        if h.engine.session(tid)['attempted']:
            await h.call('claude/runtime/close', {'threadId': tid})
            if instructions is not None:
                h.store.db.execute('UPDATE threads SET instructions=? WHERE id=?', (instructions, tid))
            await h.call('claude/runtime/start', {'threadId': tid})
        await h.turn(tid, label); await drain()
        request = next(r for r in reversed(requests) if label in json.dumps(r['messages'][-1]))
        return [m for m in MARKERS if m in json.dumps(request['system'])]

    try:
        await h.init()
        tid = await h.thread()
        h.store.db.execute('UPDATE threads SET instructions=? WHERE id=?', (MARKERS[0], tid))
        assert await ask(tid, 'T1') == ['ROLE_ALPHA_17']
        assert await ask(tid, 'T2') == ['ROLE_ALPHA_17']          # Unchanged: the record is reused.
        assert await ask(tid, 'T3', MARKERS[1]) == ['ROLE_BETA_29']  # Changed: rendered fresh.
        assert await ask(tid, 'T4') == ['ROLE_BETA_29']          # Still the new text on the next resume.

        # Control: claiming the record is current shows what the CLI does with the snapshot on.
        props = h.store.thread_properties(tid)
        h.store.set_thread_properties(tid, {**props, PROMPT_RECORD: {'digest': prompt_digest(
            h.engine.settings.preset_append(MARKERS[1]))}})
        assert await ask(tid, 'T5') == ['ROLE_ALPHA_17']          # The first request's record, not the launch text.
        h.store.set_thread_properties(tid, props)

        # A compaction re-records the launch prompt; later resumes keep the snapshot again.
        await h.call('thread/compact/start', {'threadId': tid}); await drain()
        assert h.store.thread_properties(tid)[PROMPT_RECORD]['digest'] == prompt_digest(h.engine.settings.preset_append(MARKERS[1]))
        assert await ask(tid, 'T6') == ['ROLE_BETA_29']
        assert 'snapshot' not in h.engine.driver.owners[tid].client.options.system_prompt
        print(json.dumps({'probe': 'real-sdk-cli-loopback', 'kind': 'system-prompt-resume', 'modelRequests': len(requests),
                          'changedPromptReachedModel': True, 'snapshotOnSendsFirstRecord': True}))
    finally:
        await h.close()
        server.shutdown(); server.server_close(); thread.join(timeout=2)
