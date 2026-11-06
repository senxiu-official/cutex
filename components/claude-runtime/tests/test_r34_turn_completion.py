"""Terminal settlement precedes observational metrics (offline mocks only)."""
import asyncio

import pytest

from cute_claude.sdk_runtime import TurnInterrupted
from test_persistent import ph  # noqa: F401
from test_subscription import emit_reply, sh  # noqa: F401


@pytest.mark.parametrize('state', ['completed', 'failed', 'interrupted'])
@pytest.mark.parametrize('metrics_fail', [False, True])
async def test_terminal_events_and_saved_outcome_precede_slow_metrics(sh, monkeypatch, state, metrics_fail):
    entered, release = asyncio.Event(), asyncio.Event()

    async def behavior(**kw):
        if state == 'interrupted':
            raise TurnInterrupted('mock interruption')
        overrides = {'is_error': True, 'subtype': 'error_max_turns'} if state == 'failed' else {}
        await emit_reply(kw['on_message'], kw['session_id'], 'mock reply', **overrides)

    async def refresh(tid, *, expected_turn_id=None):
        assert expected_turn_id == turn
        entered.set()
        await release.wait()
        if metrics_fail:
            raise RuntimeError('mock metrics failure')

    sh.provider.behavior = behavior
    tid = await sh.thread()
    monkeypatch.setattr(sh.engine.metrics, 'refresh_context', refresh)
    turn = await sh.turn(tid, 'first')
    try:
        await asyncio.wait_for(entered.wait(), 5)
        saved = sh.store.turn(turn)
        assert saved['status'] == state
        terminal = [m for m in sh.out if m.get('method') in {
            'turn/completed', 'thread/status/changed', 'claude/input/statusChanged'}]
        assert [m['method'] for m in terminal[-3:]] == [
            'turn/completed', 'thread/status/changed', 'claude/input/statusChanged']
        assert terminal[-3]['params']['turn'] == saved
        assert terminal[-2]['params'] == {'threadId': tid, 'status': {'type': 'idle'}}
        assert terminal[-1]['params']['state'] == state
        durable = sh.store.events(tid, 0, 1000)
        assert [e['method'] for e in durable][-3:] == [m['method'] for m in terminal[-3:]]
        assert tid in sh.engine.workers, 'metrics still serialize the next worker query'
    finally:
        release.set()
    await sh.drain()
    assert sh.store.turn(turn) == saved
    assert len([m for m in sh.out if m.get('method') == 'turn/completed']) == 1


async def test_slow_metrics_leave_no_finished_turn_to_steer_and_accept_next_input(ph, monkeypatch):
    entered, release = asyncio.Event(), asyncio.Event()
    calls = 0

    async def refresh(tid, *, expected_turn_id=None):
        assert expected_turn_id is not None
        assert ph.store.turn(expected_turn_id)['status'] == 'completed'
        nonlocal calls
        calls += 1
        if calls == 1:
            entered.set()
            await release.wait()

    tid = await ph.thread()
    monkeypatch.setattr(ph.engine.metrics, 'refresh_context', refresh)
    first = await ph.turn(tid, 'first')
    try:
        await asyncio.wait_for(entered.wait(), 5)
        assert ph.store.turn(first)['status'] == 'completed'
        rejected = await ph.call('turn/steer', {
            'threadId': tid, 'expectedTurnId': first, 'clientUserMessageId': 'late-steer',
            'input': [{'type': 'text', 'text': 'too late'}]}, error=True)
        assert rejected['code'] == -32600 and 'no active turn' in rejected['message']
        assert not ph.store.input_known(tid, 'steer-late-steer')
        second = await ph.turn(tid, 'second')
        assert second != first
        assert ph.sdk.clients[0].query_count == 1
    finally:
        release.set()
    await ph.drain()
    assert ph.store.turn(second)['status'] == 'completed'
    assert ph.sdk.clients[0].query_count == 2


@pytest.mark.parametrize('client_id', [None, 'owner-client-r34'])
async def test_initial_context_receipt_echoes_client_id(sh, client_id):
    tid = await sh.thread()
    params = {'threadId': tid, 'input': [{'type': 'text', 'text': 'initial input'}]}
    if client_id is not None:
        params['clientUserMessageId'] = client_id
    turn = (await sh.call('turn/start', params))['turn']['id']
    await sh.drain()
    receipts = [m['params'] for m in sh.out if m.get('method') == 'claude/input/contextPersisted']
    [receipt] = receipts
    mid = sh.store.db.execute('SELECT message_id FROM inputs WHERE turn_id=?', (turn,)).fetchone()[0]
    assert receipt == {'threadId': tid, 'turnId': turn, 'messageId': mid, 'clientId': client_id}
    persisted = [e['params'] for e in sh.store.events(tid, 0, 1000)
                 if e['method'] == 'claude/input/contextPersisted']
    assert persisted == receipts


async def test_terminal_callback_refuses_steer_before_durable_finish(ph, monkeypatch):
    entered, release = asyncio.Event(), asyncio.Event()
    tid = await ph.thread()
    notify = ph.engine.notify

    async def gate(thread, method, params):
        if method == 'claude/input/contextPersisted':
            entered.set()
            await release.wait()
        await notify(thread, method, params)

    monkeypatch.setattr(ph.engine, 'notify', gate)
    turn = await ph.turn(tid, 'finish this')
    try:
        await asyncio.wait_for(entered.wait(), 5)
        owner = ph.engine.driver.owners[tid]
        assert owner.finishing_turn_id == turn
        assert not owner.bridge.accepting
        assert ph.store.turn(turn)['status'] == 'inProgress'
        rejected = await ph.call('turn/steer', {
            'threadId': tid, 'expectedTurnId': turn, 'clientUserMessageId': 'finishing-steer',
            'input': [{'type': 'text', 'text': 'too late'}]}, error=True)
        assert rejected['code'] == -32009
        assert rejected['data']['reason'] == 'turnFinishing'
        assert not ph.store.input_known(tid, 'steer-finishing-steer')
    finally:
        release.set()
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed'
    following = await ph.turn(tid, 'fresh input')
    await ph.drain()
    assert ph.store.turn(following)['status'] == 'completed'
    assert ph.sdk.clients[0].query_count == 2
