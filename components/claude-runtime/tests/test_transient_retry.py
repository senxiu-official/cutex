import asyncio
from dataclasses import replace
import json

import pytest

from cute_claude.subscription import SdkResponseError
from cute_claude.transient_retry import retry_delay
from test_persistent import ph, until


def failure(text, code='server_error', **extra):
    return {'type': 'assistant', 'error': code, 'content': [{'text': text}], **extra}


async def install_query(ph, messages, barrier=None):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]

    async def query(prompt):
        client.query_count += 1
        if barrier:
            await barrier(tid, client)
        msg = messages[min(client.query_count - 1, len(messages) - 1)]
        await client.events.put(msg)
        await client.events.put({'type': 'result', 'session_id': ph.engine.session(tid)['session_id'],
                                 'subtype': 'error_during_execution' if msg.get('error') else 'success',
                                 'is_error': bool(msg.get('error')), 'num_turns': 1, 'usage': {}})

    client.query = query
    return tid, client


@pytest.mark.parametrize('text', ['API Error: Connection dropped (ECONNRESET).',
                                  "API Error: Can't reach the API server (ENOTFOUND)",
                                  'API Error: Request timed out (ETIMEDOUT)',
                                  'API Error: 500 internal error', 'HTTP 529 overloaded_error'])
async def test_explicit_silent_transient_failure_then_success_in_same_turn(ph, monkeypatch, text):
    monkeypatch.setattr('cute_claude.transient_retry.random.uniform', lambda *args: 0)
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda error, n: 0 if n < 2 else None)
    tid, client = await install_query(ph, [failure(text), {'type': 'assistant', 'content': [{'type': 'text', 'text': 'done'}]}])
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed'
    assert client.query_count == 2
    assert len([e for e in ph.out if e.get('method') == 'turn/completed']) == 1
    assert client.options.env['CLAUDE_CODE_MAX_RETRIES'] == '0'


async def test_exhaustion_is_three_total_attempts(ph, monkeypatch):
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda e, n: 0 if n < 2 else None)
    tid, client = await install_query(ph, [failure('API Error: 500 internal error')])
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'failed' and client.query_count == 3


@pytest.mark.parametrize('code,text', [('authentication_failed', 'API Error: 401 refused'),
                                     ('invalid_request', 'API Error: 400 refused'),
                                     ('rate_limit', 'API Error: 429 wait'),
                                     ('billing_error', 'usage limit reached'),
                                     ('server_error', 'API Error: 429 wait'),
                                     ('server_error', 'unknown error')])
async def test_non_transient_failure_is_never_retried(ph, code, text):
    tid, client = await install_query(ph, [failure(text, code)])
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'failed' and client.query_count == 1


@pytest.mark.parametrize('kind', ['text', 'stream', 'receipt', 'tool', 'hook'])
async def test_any_output_tool_receipt_or_hook_blocks_replay(ph, kind):
    async def barrier(tid, client):
        if kind == 'text':
            await client.events.put({'type': 'assistant', 'content': [{'type': 'text', 'text': 'a long preceding reply'}]})
        elif kind == 'stream':
            await client.events.put({'type': 'stream_event', 'event': {'type': 'content_block_start', 'index': 0,
                                      'content_block': {'type': 'thinking', 'thinking': ''}}})
        elif kind == 'receipt':
            await ph.engine.emit(tid, 'claude/input/contextPersisted', {'threadId': tid, 'messageId': 'once'})
        elif kind == 'tool':
            await ph.engine.emit(tid, 'item/started', {'threadId': tid, 'turnId': ph.engine.retry_turns[tid],
                                                     'item': {'type': 'commandExecution', 'id': 'once'}})
        elif kind == 'hook':
            await client.events.put({'type': 'system', 'subtype': 'hook_started'})
    tid, client = await install_query(ph, [failure('API Error: Connection dropped (ECONNRESET).')], barrier)
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'failed' and client.query_count == 1
    if kind == 'receipt':
        assert len([e for e in ph.out if e.get('method') == 'claude/input/contextPersisted' and e['params'].get('messageId') == 'once']) == 1


async def test_cancel_interrupts_backoff_without_another_query(ph, monkeypatch):
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda *args: 60)
    tid, client = await install_query(ph, [failure('API Error: 500 internal error')])
    turn = await ph.turn(tid, 'original')
    await until(lambda: turn in ph.engine.retry_waiting)
    async with asyncio.timeout(1):
        await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert ph.store.turn(turn)['status'] == 'interrupted' and client.query_count == 1
    assert not ph.engine.retry_waiting


async def test_close_cancels_backoff_before_closing_the_idle_native(ph, monkeypatch):
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda *args: 60)
    tid, client = await install_query(ph, [failure('API Error: 500 internal error')])
    turn = await ph.turn(tid, 'original')
    await until(lambda: turn in ph.engine.retry_waiting)
    async with asyncio.timeout(1):
        await ph.engine.close()
    assert ph.store.turn(turn)['status'] == 'interrupted' and client.query_count == 1


def test_backoff_floor_jitter_budget_and_retry_after(monkeypatch):
    e = SdkResponseError(failure('API Error: 500 temporary'), None)
    monkeypatch.setattr('cute_claude.transient_retry.random.uniform', lambda lo, hi: hi)
    assert retry_delay(e, 0) == 1.25
    assert retry_delay(e, 1) == 2.5
    assert retry_delay(e, 2) is None
    e = SdkResponseError(failure('API Error: 503 temporary', headers={'Retry-After': '5'}), None)
    assert retry_delay(e, 0) == 5
    e = SdkResponseError(failure('API Error: 503 temporary', headers={'Retry-After': '120'}), None)
    assert retry_delay(e, 0) is None


async def test_receipt_arriving_during_backoff_vetoes_next_query(ph, monkeypatch):
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda *args: 0.05)
    tid, client = await install_query(ph, [failure('API Error: 500 internal error')])
    turn = await ph.turn(tid, 'original')
    await until(lambda: turn in ph.engine.retry_waiting)
    await ph.engine.emit(tid, 'claude/input/contextPersisted', {'threadId': tid, 'messageId': 'late-once'})
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'failed' and client.query_count == 1


async def test_uncertain_stream_outcome_is_never_replayed(ph, monkeypatch):
    from cute_claude.sdk_runtime import OutcomeUncertain
    tid = await ph.thread()
    attempts = []

    async def run(**kwargs):
        attempts.append(True)
        raise OutcomeUncertain('stream ended without a terminal result')

    monkeypatch.setattr(ph.engine.driver, 'run', run)
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert len(attempts) == 1 and ph.store.turn(turn)['status'] == 'failed'
    assert ph.store.db.execute('SELECT state FROM inputs WHERE turn_id=?', (turn,)).fetchone()[0] == 'uncertain'


@pytest.mark.parametrize('configured', [False, True])
async def test_project_settings_gate_only_when_hooks_are_configured(ph, monkeypatch, configured):
    ph.engine.settings = replace(ph.engine.settings, setting_sources=('project',), allow_exec=True)
    folder = ph.engine.settings.workspace / '.claude'
    folder.mkdir()
    settings = {'hooks': {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': 'echo hook'}]}]}} if configured else {'permissions': {}}
    (folder / 'settings.json').write_text(json.dumps(settings))
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda e, n: 0 if n < 2 else None)
    tid, client = await install_query(ph, [failure('API Error: 500 temporary'),
                                         {'type': 'assistant', 'content': [{'type': 'text', 'text': 'done'}]}])
    turn = await ph.turn(tid, 'original')
    await ph.drain()
    assert client.query_count == (1 if configured else 2)
    assert ph.store.turn(turn)['status'] == ('failed' if configured else 'completed')


async def test_stop_between_terminal_result_and_worker_resume_skips_backoff(ph, monkeypatch):
    monkeypatch.setattr('cute_claude.transient_retry.retry_delay', lambda *args: 60)
    tid, client = await install_query(ph, [failure('API Error: 500 temporary')])
    driver = ph.engine.driver
    run = driver.run
    terminal, release = asyncio.Event(), asyncio.Event()

    async def paused_run(**kwargs):
        await run(**kwargs)
        terminal.set()
        await release.wait()

    monkeypatch.setattr(driver, 'run', paused_run)
    turn = await ph.turn(tid, 'original')
    await terminal.wait()
    stop = asyncio.create_task(ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn}))
    await until(lambda: turn in driver.owners[tid].cancelled_turns)
    release.set()
    async with asyncio.timeout(1):
        await stop
    assert ph.store.turn(turn)['status'] == 'interrupted'
    assert client.query_count == 1 and not ph.engine.retry_waiting


async def test_stop_interrupts_refresh_lock_backoff(ph, monkeypatch):
    monkeypatch.setenv('CUTE_CLAUDE_REFRESH_RETRY_DELAYS', '60')
    from test_claude_p0 import REFRESH_TEXT
    tid, client = await install_query(ph, [failure(REFRESH_TEXT)])
    turn = await ph.turn(tid, 'original')
    await until(lambda: turn in ph.engine.retry_waiting)
    async with asyncio.timeout(1):
        await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert ph.store.turn(turn)['status'] == 'interrupted' and client.query_count == 1
