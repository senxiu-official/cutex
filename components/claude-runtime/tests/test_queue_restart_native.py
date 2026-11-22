"""Bundled CLI restart proof: retained empty transport input, native history only.

Only the synthetic model endpoint is used. No engine Stop/close is issued at
restart: it would intentionally terminate the retained input being tested.
"""
import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib
import hashlib
import json
import os
from pathlib import Path
import threading

import pytest

from cute_claude.config import Settings
from cute_claude.queue_recovery import read
from cute_claude.subscription import OfficialSdkDriver
from cute_claude.subscription_auth import CONFLICTING_ENV
from cute_claude.transient_retry import RETRY_CONTINUATION
from fake_sdk import materialize
from test_retry_history_native import sse
from test_subscription import SubHarness


pytestmark = pytest.mark.skipif(
    not os.environ.get('CUTEX_TEST_CLAUDE_CLI'),
    reason='explicit private bundled native CLI required',
)
PROMPT = 'RESTART-EMPTY-INPUT-92473 reply briefly.'


def _text(content):
    if isinstance(content, str):
        return content
    return '\n'.join(block.get('text', '') for block in content or []
                     if isinstance(block, dict) and block.get('type') == 'text')


def _history(config, session_id):
    paths = list((config / 'projects').glob(f'*/{session_id}.jsonl'))
    assert len(paths) == 1, 'one private native transcript must own the session'
    return [json.loads(line) for line in paths[0].read_text(encoding='utf-8').splitlines()
            if line.strip()]


def _digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True,
                                    separators=(',', ':')).encode()).hexdigest()


def _identity(store):
    return [tuple(row) for row in store.db.execute(
        'SELECT seq,thread_id,message_id,semantic_hash,payload,turn_id FROM inputs ORDER BY seq')]


def _retry(store, turn_id):
    row = store.db.execute('SELECT value FROM meta WHERE key=?',
                           ('claudeQueueRetry:' + turn_id,)).fetchone()
    assert row is not None, 'retained input must have a durable retry receipt'
    return json.loads(row[0])


async def _wait_for(predicate, seconds=60):
    async with asyncio.timeout(seconds):
        while not predicate():
            await asyncio.sleep(0.01)


async def _release_writer(harness):
    # Cancellation is safe only at the queue's idle backoff, never during SDK IO.
    assert harness.engine.queue_backoffs
    workers = list(harness.engine.workers.values())
    assert workers
    for worker in workers:
        worker.cancel()
    await asyncio.gather(*workers, return_exceptions=True)
    assert not harness.engine.workers
    owners = list(harness.engine.driver.owners.values())
    assert owners and all(owner.active is None for owner in owners)
    await harness.engine.driver.close()
    assert all(owner.task.done() and owner.process_exit_confirmed for owner in owners), (
        'the old CLI must be reaped before another writer/native owner starts')
    await harness.tools.close()
    harness.store.close()


async def test_actual_cli_retained_input_auto_recovers_after_adapter_restart(tmp_path, monkeypatch):
    for name in CONFLICTING_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ('HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP', 'TMPDIR'):
        folder = tmp_path / name.lower()
        folder.mkdir(exist_ok=True)
        monkeypatch.setenv(name, str(folder))
    workspace = tmp_path / 'workspace'
    workspace.mkdir()
    config = tmp_path / 'native-config'
    settings = Settings(workspace, tmp_path / 'state', model='claude-sonnet-4-6',
                        mock=False, backend='subscription', thinking='disabled')
    monkeypatch.setattr('cute_claude.queue_recovery.BASE_DELAY', 60)
    recovered_api = threading.Event()
    bodies, native_inputs, calls = [], [], []
    second = None
    first = None
    first_closed = False
    required_responses = {}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def respond(self, status, content_type, data):
            self.send_response(status)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(data)))
            self.send_header('Retry-After', '0')
            self.end_headers()
            self.wfile.write(data)
            self.wfile.flush()

        def do_POST(self):
            self.connection.setblocking(True)
            raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            if not self.path.split('?')[0].endswith('/messages'):
                self.respond(200, 'application/json', b'{"input_tokens":1}')
                return
            healthy = recovered_api.is_set()
            bodies.append((healthy, json.loads(raw)))
            if not healthy:
                self.respond(500, 'application/json', json.dumps({
                    'type': 'error', 'error': {'type': 'api_error',
                    'message': 'synthetic private outage before adapter restart'},
                }).encode())
                return
            self.respond(200, 'text/event-stream', sse(len(bodies), 'restart fixture completed'))

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    sdk = importlib.import_module('claude_agent_sdk')
    native_client = sdk.ClaudeSDKClient

    def client_for_fixture(*, options):
        # Instrument the subscription child only; never use actual API/OAuth auth.
        assert options.env['CLAUDE_CODE_MAX_RETRIES'] == '0'
        options.env.update({
            'ANTHROPIC_API_KEY': 'synthetic-fixture-key',
            'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{server.server_port}',
            'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1',
            'DISABLE_TELEMETRY': '1', 'DISABLE_ERROR_REPORTING': '1',
        })
        client = native_client(options=options)
        original = client.query

        async def query(prompt, *args, **kwargs):
            if second is not None:
                # A query must follow both *protocol responses*, not just method entry.
                assert set(required_responses) == {'runtime', 'gate'}
                responses = {message.get('id'): message for message in second.out
                             if 'id' in message}
                for rid in required_responses.values():
                    assert 'result' in responses.get(rid, {}), 'native query preceded startup permission response'
                assert responses[required_responses['gate']]['result']['held'] is False
            text, replay = await materialize(prompt)
            native_inputs.append(text)
            return await original(replay, *args, **kwargs)

        client.query = query
        return client

    # Install before initialize/start: an uninstrumented client must never exist.
    monkeypatch.setattr(sdk, 'ClaudeSDKClient', client_for_fixture)
    driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), config)
    first = SubHarness(settings, driver)
    try:
        await first.init()
        tid = await first.thread()
        turn = await first.turn(tid, PROMPT)
        await _wait_for(lambda: tid in first.engine.queue_backoffs)
        assert len(bodies) == 1 and not bodies[0][0]
        assert len(native_inputs) == 1 and native_inputs[0].count(PROMPT) == 1
        identities = _identity(first.store)
        assert len(identities) == 1 and identities[0][-1] == turn
        assert first.store.turn(turn)['status'] == 'inProgress'
        assert not first.store.thread(tid)['halted']
        assert read(first.store, tid)['count'] == 0 and not read(first.store, tid)['held']
        not_before = read(first.store, tid)['notBefore']
        old_retry = _retry(first.store, turn)
        receipt = old_retry['receipt']
        assert old_retry['dispatch'] == 'terminal-empty' and old_retry['continuation']
        assert receipt['state'] == 'terminal-empty' and receipt['turnId'] == turn
        assert receipt['threadId'] == tid and receipt['history']
        sid = first.engine.session(tid)['session_id']
        assert receipt['sessionId'] == sid
        assert receipt['input'] == [identities[0][2], identities[0][0], identities[0][3]]
        entries = _history(config, sid)
        original_users = [entry for entry in entries if entry.get('type') == 'user'
                          and PROMPT in _text(entry.get('message', {}).get('content'))]
        assert len(original_users) == 1
        user_uuid = original_users[0]['uuid']
        error_uuid = receipt['errorUuid']
        by_uuid = {entry['uuid']: entry for entry in entries if entry.get('uuid')}
        assert by_uuid[error_uuid]['isApiErrorMessage']
        walked, chain = set(), []
        cursor = error_uuid
        while cursor in by_uuid:
            assert cursor not in walked, 'native parent chain must not cycle'
            walked.add(cursor)
            row = by_uuid[cursor]
            assert not row.get('isSidechain')
            assert row['sessionId'] == sid
            assert row.get('type') in {'user','attachment'} or row.get('isApiErrorMessage') is True
            chain.append([cursor, _digest(row)])
            if cursor == user_uuid:
                break
            cursor = row.get('parentUuid')
        assert user_uuid in walked, 'native terminal error must descend from the original user'
        assert receipt['history'] == {'anchor': user_uuid, 'tip': error_uuid, 'sha256': _digest(chain)}, (
            'persisted proof must bind the original user, terminal error and exact native chain')
        await _release_writer(first)
        first_closed = True
        before_events = list(first.out)
        assert first.engine.closing is False, 'restart must not invoke engine Stop/close'
        # Only advance the elapsed backoff clock; never rewrite the receipt/history.
        monkeypatch.setattr('cute_claude.queue_recovery.time_now', lambda: not_before + 1)
        recovered_api.set()
        second_driver = OfficialSdkDriver(Path(os.environ['CUTEX_TEST_CLAUDE_CLI']), config)
        second = SubHarness(settings, second_driver)  # Reopen the same released writer lock.
        await second.init()
        assert _identity(second.store) == identities
        assert second.store.turn(turn)['status'] == 'inProgress'
        # Candidate remains internally fenced until native lease/history and gate replay validate.
        assert second.store.recovered_probes.get(tid) == turn
        await second.call('thread/resume', {'threadId': tid})
        await second.call('thread/read', {'threadId': tid, 'includeTurns': True})
        await asyncio.sleep(0.05)
        assert len(native_inputs) == len(bodies) == 1, 'read/resume must remain model-read-only'

        required_responses['runtime'] = second.next_id + 1
        calls.append('claude/runtime/start')
        await second.call('claude/runtime/start', {'threadId': tid})
        await asyncio.sleep(0.05)
        assert len(native_inputs) == len(bodies) == 1, 'runtime start alone must await daemon tool gate decision'
        required_responses['gate'] = second.next_id + 1
        calls.append('claude/toolGate/set')
        await second.call('claude/toolGate/set', {'threadId': tid, 'held': False})
        await _wait_for(lambda: second.store.turn(turn)['status'] == 'completed', seconds=90)
        while second.engine.workers:
            async with asyncio.timeout(5):
                await asyncio.gather(*list(second.engine.workers.values()))

        assert calls == ['claude/runtime/start', 'claude/toolGate/set']
        assert len(native_inputs) == 2 and native_inputs[1] == RETRY_CONTINUATION
        assert sum(payload.count(PROMPT) for payload in native_inputs) == 1
        assert len(bodies) == 2 and [healthy for healthy, _ in bodies] == [False, True]
        for _, body in bodies:
            text = '\n'.join(_text(message.get('content')) for message in body['messages'])
            assert text.count(PROMPT) == 1, 'API history must not duplicate the original input'
        final_text = '\n'.join(_text(message.get('content')) for message in bodies[-1][1]['messages'])
        assert final_text.count(RETRY_CONTINUATION) == 1
        final_history = _history(config, sid)
        assert len([entry for entry in final_history if entry.get('type') == 'user'
                    and PROMPT in _text(entry.get('message', {}).get('content'))]) == 1
        assert second.engine.session(tid)['session_id'] == sid
        assert _identity(second.store) == identities
        assert second.store.db.execute('SELECT COUNT(*) FROM turns').fetchone()[0] == 1
        assert second.store.db.execute("SELECT state FROM inputs WHERE turn_id=?", (turn,)).fetchone()[0] == 'completed'
        assert not second.store.db.execute("SELECT 1 FROM turns WHERE state IN ('failed','interrupted')").fetchone()
        assert not second.store.thread(tid)['halted'] and read(second.store, tid) is None
        events = before_events + second.out
        assert not [event for event in events if event.get('method') == 'claude/queue/holdChanged'
                    and event['params'].get('held')]
        assert len([event for event in events if event.get('method') == 'turn/started']) == 1
        completed = [event['params'] for event in events if event.get('method') == 'turn/completed']
        assert len(completed) == 1 and completed[0]['turn']['id'] == turn
        print(json.dumps({'restart': 'native-terminal-empty', 'httpAttempts': len(bodies),
                          'nativeQueries': len(native_inputs), 'inputRows': len(identities),
                          'originalUserUuid': user_uuid, 'errorUuid': error_uuid,
                          'turnId': turn, 'status': second.store.turn(turn)['status']}))
    finally:
        try:
            if second is not None:
                await second.close()
            if first is not None and not first_closed:
                # Failure cleanup may terminate unfinished input; the proved restart above never does.
                await first.close()
        finally:
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=5)
            assert not server_thread.is_alive()
