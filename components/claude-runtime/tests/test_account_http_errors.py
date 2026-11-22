"""Opt-in native HTTP smoke: private CLI + fake loopback Anthropic endpoint.

Set CUTEX_TEST_CLAUDE_CLI to an explicitly selected test-only Claude executable.
No credential discovery/login is performed; only a synthetic API key is used.
"""
import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import threading

import pytest

from cute_claude.queue_recovery import read
from cute_claude.config import Settings
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import ApiAccount, CONFLICTING_ENV
from test_subscription import SubHarness


@pytest.mark.skipif(not os.environ.get('CUTEX_TEST_CLAUDE_CLI'), reason='private native CLI not explicitly selected')
@pytest.mark.parametrize('status,kind,info,attempts', [
    # The pinned native CLI also retries API 401 under MAX_RETRIES=3; with the key
    # from apiKeyHelper (adapter 0.7.1) a 401 also re-runs the helper and retries
    # twice more (CLI 2.1.299: 6 attempts; with the key in the env: 4).
    (401, 'authentication_error', 'unauthorized', 6),
    (429, 'rate_limit_error', 'rateLimitExceeded', 4),
    (500, 'api_error', 'internalServerError', 4),
    (529, 'overloaded_error', 'serverOverloaded', 4),
])
async def test_native_http_retry_count_and_transport_outage_keeps_retrying(tmp_path, monkeypatch, status, kind, info, attempts):
    calls = []
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args): pass
        def do_POST(self):
            self.rfile.read(int(self.headers.get('Content-Length', 0)))
            if self.path.split('?')[0].endswith('/messages/count_tokens'):
                body, code = {'input_tokens': 1}, 200
            else:
                calls.append(self.path.split('?')[0])
                body, code = {'type': 'error', 'error': {'type': kind, 'message': f'HTTP {status} private fixture body'}}, status
            raw = json.dumps(body).encode()
            self.send_response(code)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(raw)))
            self.send_header('Retry-After', '0')
            self.end_headers()
            self.wfile.write(raw)
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    serving = threading.Thread(target=server.serve_forever, daemon=True)
    serving.start()
    private_home = tmp_path / 'home'
    private_home.mkdir()
    monkeypatch.setenv('HOME', str(private_home))
    monkeypatch.setenv('USERPROFILE', str(private_home))
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    key_file = tmp_path / 'keys' / 'ca-loopback.json'
    key_file.parent.mkdir()
    key_file.write_text(json.dumps({'apiKey': 'synthetic-fixture-key'}), encoding='utf-8')
    account = ApiAccount('ca-loopback', 'ANTHROPIC_API_KEY', f'http://127.0.0.1:{server.server_port}', {
        'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_TELEMETRY': '1',
        'DISABLE_ERROR_REPORTING': '1', 'ANTHROPIC_DEFAULT_SONNET_MODEL': 'fixture-model',
    }, 'synthetic-fixture-key', key_file)
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), private_home / 'claude', api_account=account)
    h = SubHarness(Settings(workspace, tmp_path / 'state', model='sonnet', mock=False, backend='subscription'), driver)
    backoff_observed = asyncio.Event()

    async def fixture_backoff(event, delay):
        # Advance the first two adapter backoffs without production wall time;
        # freeze the third so the retained queue can be inspected and cancelled.
        if len(calls) >= attempts * 3:
            backoff_observed.set()
            await event.wait()
        else:
            await asyncio.sleep(0)

    if status >= 500:
        monkeypatch.setattr('cute_claude.queue_recovery.wait_backoff', fixture_backoff)
        monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 60)
    try:
        await h.init()
        tid = await h.thread()
        turn = await h.turn(tid, 'Reply briefly to this local fixture.')
        original = dict(h.store.db.execute("SELECT * FROM inputs WHERE turn_id=?", (turn,)).fetchone())
        async with asyncio.timeout(120):
            if status >= 500:
                while not backoff_observed.is_set():
                    assert h.engine.workers, f'outage stopped retrying: {h.store.turn(turn)}'
                    assert not h.store.thread(tid)['halted'], h.store.turn(turn)
                    await asyncio.sleep(0.01)
            else:
                while h.engine.workers:
                    await asyncio.gather(*list(h.engine.workers.values()))
        result = h.store.turn(turn)
        diagnostic = result.get('claudeRetry', {}).get('error') or result['error']
        if status >= 500:
            assert result['error'] is None and result['claudeRetry']['willRetry']
        evidence = {'httpStatus': status, 'attempts': len(calls), 'paths': calls,
                    'turnStatus': result['status'], 'error': diagnostic}
        print(json.dumps(evidence))
        assert result['status'] == ('inProgress' if status >= 500 else 'failed'), evidence
        if status >= 500:
            assert h.store.db.execute("SELECT state FROM inputs WHERE turn_id=?", (turn,)).fetchone()[0] == 'queued'
        assert diagnostic['codexErrorInfo'] == info, evidence
        assert len(calls) == attempts * (3 if status >= 500 else 1), evidence
        assert 'Official Claude' not in diagnostic['message']
        assert 'private fixture body' not in diagnostic['message']
        assert 'synthetic-fixture-key' not in json.dumps(evidence)
        if status >= 500:
            retained = dict(h.store.db.execute("SELECT * FROM inputs WHERE turn_id=?", (turn,)).fetchone())
            assert {k: v for k, v in retained.items() if k != 'state'} == {k: v for k, v in original.items() if k != 'state'}
            assert h.store.db.execute("SELECT COUNT(*) FROM inputs WHERE turn_id=?", (turn,)).fetchone()[0] == 1
            assert not h.store.thread(tid)['halted']
            assert read(h.store, tid)['probeCount'] >= 3 and not read(h.store, tid)['held']
            assert any(e.get('method') == 'error' and e['params'].get('willRetry') for e in h.out)
            assert not any(e.get('method') == 'claude/queue/holdChanged' and e['params']['held'] for e in h.out)
            assert not any(e.get('method') == 'turn/completed' for e in h.out)
            async with asyncio.timeout(5):
                await h.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
                await h.drain()
            assert h.store.turn(turn)['status'] == 'interrupted'
            assert len(calls) == attempts * 3, 'interrupt during backoff must not start another native query'
    finally:
        await h.close()
        server.shutdown()
        server.server_close()
        serving.join()
