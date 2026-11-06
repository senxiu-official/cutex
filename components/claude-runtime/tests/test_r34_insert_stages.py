"""r34 insert progress and notification holds through the real offline runtime.

Only the SDK boundary is a fixture: protocol admission, SQLite, authenticated
hooks, owner mailbox, terminal callbacks and event publication are production.
"""
import asyncio
import json

import pytest

import cute_claude.sdk_runtime as sdk_runtime
from cute_claude.busy_delivery import BusyDelivery, STEER_FRAME
from test_autonomous_turns import autonomous, started
from test_perms_steer import post_hook, running_turn, steer
from test_persistent import ph, until  # noqa: F401 (shared integrated fixture)


STAGE = 'claude/input/stageChanged'
CONTEXT = 'claude/input/contextPersisted'


def live(ph, method, client_id=None):
    return [m['params'] for m in ph.out if m.get('method') == method
            and (client_id is None or m['params'].get('clientId') == client_id)]


def saved(ph, tid, method, client_id=None):
    return [e['params'] for e in ph.store.events(tid, 0, 1000)
            if e['method'] == method
            and (client_id is None or e['params'].get('clientId') == client_id)]


async def publications(ph):
    # Await the actual publisher chain, including its per-thread predecessor.
    async with asyncio.timeout(5):
        while ph.server.busy_delivery.stage_tasks:
            await asyncio.gather(*list(ph.server.busy_delivery.stage_tasks.values()))
            await asyncio.sleep(0)


async def result(ph, tid, *, failed=False):
    client = ph.sdk.clients[0]
    await client.events.put({
        'type': 'result', 'session_id': ph.engine.session(tid)['session_id'],
        'subtype': 'error_max_turns' if failed else 'success', 'is_error': failed,
        'terminal_reason': 'max_turns' if failed else 'completed',
        'num_turns': 1, 'usage': {},
    })
    await ph.drain()
    await publications(ph)


def expected(tid, turn, client, stages):
    return [{'threadId': tid, 'turnId': turn, 'messageId': 'steer-' + client,
             'clientId': client, 'stage': stage} for stage in stages]


async def test_stages_commit_synchronously_publish_in_order_once_and_replay(ph, monkeypatch):
    tid, turn, bridge = await running_turn(ph)
    entered, release = asyncio.Event(), asyncio.Event()
    notify = ph.engine.notify

    async def delayed(thread, method, params):
        if method == STAGE and params['clientId'] == 'ordered' and params['stage'] == 'registered':
            entered.set()
            await release.wait()
        await notify(thread, method, params)

    monkeypatch.setattr(ph.engine, 'notify', delayed)
    params = steer(tid, client='ordered', expectedTurnId=turn)
    try:
        await ph.call('turn/steer', params)
        assert saved(ph, tid, STAGE, 'ordered') == expected(tid, turn, 'ordered', ['registered'])
        await asyncio.wait_for(entered.wait(), 5)
        assert (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext'].startswith(STEER_FRAME)
        assert saved(ph, tid, STAGE, 'ordered') == expected(tid, turn, 'ordered', ['registered', 'offered'])
        # Drive the real successful terminal callback while the first live
        # publication is blocked; later stages must remain behind it.
        await ph.sdk.clients[0].events.put({
            'type': 'result', 'session_id': bridge.sid, 'subtype': 'success',
            'is_error': False, 'terminal_reason': 'completed', 'usage': {}, 'num_turns': 1})
        await ph.drain()
        want = expected(tid, turn, 'ordered', ['registered', 'offered', 'contextPersisted'])
        assert saved(ph, tid, STAGE, 'ordered') == want
        assert live(ph, STAGE, 'ordered') == []
    finally:
        release.set()
    await publications(ph)
    assert live(ph, STAGE, 'ordered') == want
    assert live(ph, CONTEXT, 'ordered') == [{k: v for k, v in want[-1].items() if k != 'stage'}]
    assert saved(ph, tid, CONTEXT, 'ordered') == live(ph, CONTEXT, 'ordered')

    # A fresh subscriber replays the journal, including a cursor that splits
    # the stage sequence, rather than inferring progress from the final row.
    ph.server.disconnect(ph.peer)
    ph.peer = ph.server.connect(ph.send)
    await ph.init()
    replay = await ph.call('claude/events/read', {'threadId': tid, 'limit': 200})
    events = [e for e in replay['data'] if e['method'] == STAGE and e['params']['clientId'] == 'ordered']
    assert [e['params'] for e in events] == want
    tail = await ph.call('claude/events/read', {
        'threadId': tid, 'afterSequence': events[0]['sequence'], 'limit': 200})
    assert [e['params'] for e in tail['data'] if e['method'] == STAGE] == want[1:]


async def test_rolled_back_registration_cannot_publish_retried_event_with_reused_sequence(ph):
    tid, turn, _ = await running_turn(ph)
    client = 'rollback-retry'
    params = steer(tid, client=client, expectedTurnId=turn)
    want = expected(tid, turn, client, ['registered'])

    with pytest.raises(RuntimeError, match='rollback registration'):
        with ph.store.transaction():
            # Use the real synchronous owner-steer admission path. Neither
            # this attempt nor its immediate retry may yield to a publisher.
            reply, _ = ph.server._steer(ph.peer, params)
            assert reply == {'turnId': turn}
            assert saved(ph, tid, STAGE, client) == want
            rolled_back_seq = ph.store.db.execute(
                'SELECT max(seq) FROM events WHERE thread_id=? AND method=?', (tid, STAGE)).fetchone()[0]
            old_publisher = ph.server.busy_delivery.stage_tasks[tid]
            assert not old_publisher.done()
            raise RuntimeError('rollback registration')

    assert not ph.store.input_known(tid, 'steer-' + client)
    assert saved(ph, tid, STAGE, client) == []
    reply, _ = ph.server._steer(ph.peer, params)
    assert reply == {'turnId': turn}
    retried_seq = ph.store.db.execute(
        'SELECT max(seq) FROM events WHERE thread_id=? AND method=?', (tid, STAGE)).fetchone()[0]
    assert retried_seq == rolled_back_seq, 'exercise SQLite sequence reuse with identical stage params'
    assert not old_publisher.done()
    assert live(ph, STAGE, client) == []
    assert saved(ph, tid, STAGE, client) == want

    await publications(ph)
    assert old_publisher.done()
    assert saved(ph, tid, STAGE, client) == live(ph, STAGE, client) == want
    assert ph.store.db.execute(
        'SELECT count(*) FROM inputs WHERE thread_id=? AND message_id=?',
        (tid, 'steer-' + client)).fetchone()[0] == 1


async def test_duplicate_client_id_and_hook_cannot_regress_or_repeat_stages(ph):
    tid, turn, bridge = await running_turn(ph)
    params = steer(tid, client='duplicate', expectedTurnId=turn)
    for _ in range(2):
        assert await ph.call('turn/steer', params) == {'turnId': turn}
    await post_hook(ph, bridge)
    # Retrying admission after the hook claim must not reset offered.
    await ph.call('turn/steer', params)
    assert 'additionalContext' not in json.dumps(await post_hook(ph, bridge, 'another-tool'))
    await publications(ph)
    assert live(ph, STAGE, 'duplicate') == expected(tid, turn, 'duplicate', ['registered', 'offered'])
    assert ph.store.db.execute("SELECT count(*) FROM inputs WHERE message_id='steer-duplicate'").fetchone()[0] == 1
    await result(ph, tid)
    # Repeated acknowledgements and a reconstructed delivery service retain
    # the durable high-water mark and never republish an older stage.
    await ph.server.busy_delivery.acknowledge(tid, turn)
    delivery = BusyDelivery(ph.server)
    row = ph.store.input_status(tid, 'steer-duplicate')
    delivery.register(tid, 'steer-duplicate', row, turn_id=turn)
    delivery.offered(tid, turn)
    await publications(ph)
    assert not delivery.stage_tasks
    want = expected(tid, turn, 'duplicate', ['registered', 'offered', 'contextPersisted'])
    assert live(ph, STAGE, 'duplicate') == saved(ph, tid, STAGE, 'duplicate') == want
    assert len(live(ph, CONTEXT, 'duplicate')) == 1


async def test_two_client_ids_keep_independent_ordered_progress_in_one_hook(ph):
    tid, turn, bridge = await running_turn(ph)
    for client in ('one', 'two'):
        await ph.call('turn/steer', steer(tid, client=client, text='insert ' + client, expectedTurnId=turn))
    context = (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext']
    assert context.index('insert one') < context.index('insert two')
    await result(ph, tid)
    for client in ('one', 'two'):
        want = expected(tid, turn, client, ['registered', 'offered', 'contextPersisted'])
        assert live(ph, STAGE, client) == saved(ph, tid, STAGE, client) == want
        assert live(ph, CONTEXT, client) == [{k: v for k, v in want[-1].items() if k != 'stage'}]


@pytest.mark.parametrize('forgery', ['unadmitted', 'wrong-session', 'child', 'duplicate-hook'])
async def test_untrusted_or_consumed_hook_never_offers_pending_insert(ph, forgery):
    tid, turn, bridge = await running_turn(ph)
    data = {'session_id': bridge.sid, 'hook_event_name': 'PostToolUse', 'tool_name': 'Read',
            'tool_input': {'file_path': str(ph.settings.workspace / 'hello.txt')}, 'tool_response': 'ok'}
    hook = ph.sdk.clients[0].options.hooks['PostToolUse'][0].hooks[0]
    if forgery != 'unadmitted':
        await bridge.pre_tool(data, 'tool-admitted', {})
    if forgery == 'duplicate-hook':
        assert await hook(data, 'tool-admitted', {}) == {}
    await ph.call('turn/steer', steer(tid, client='untrusted', expectedTurnId=turn))
    forged = {**data}
    if forgery == 'wrong-session':
        forged['session_id'] = 'forged-session'
    if forgery == 'child':
        forged['agent_id'] = 'child-agent'
    assert await hook(forged, 'tool-admitted', {}) == {}
    await publications(ph)
    assert live(ph, STAGE, 'untrusted') == expected(tid, turn, 'untrusted', ['registered'])
    assert saved(ph, tid, STAGE, 'untrusted') == live(ph, STAGE, 'untrusted')
    status = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'steer-untrusted'})
    assert status['contextPersisted'] is False and status['contextTurnId'] is None
    # A new valid admission can still consume the pending input exactly once.
    assert 'additionalContext' in (await post_hook(ph, bridge, 'fresh-tool'))['hookSpecificOutput']
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    await ph.drain()


@pytest.mark.parametrize('outcome', ['interrupted', 'failed'])
@pytest.mark.parametrize('offered', [False, True])
async def test_non_success_never_claims_context_persisted(ph, outcome, offered):
    tid, turn, bridge = await running_turn(ph)
    await ph.call('turn/steer', steer(tid, client='unfinished', expectedTurnId=turn))
    if offered:
        await post_hook(ph, bridge)
    if outcome == 'interrupted':
        await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
        await ph.drain()
        await publications(ph)
    else:
        await result(ph, tid, failed=True)
    assert ph.store.turn(turn)['status'] == outcome
    want = expected(tid, turn, 'unfinished', ['registered', 'offered'] if offered else ['registered'])
    assert live(ph, STAGE, 'unfinished') == saved(ph, tid, STAGE, 'unfinished') == want
    assert live(ph, CONTEXT) == saved(ph, tid, CONTEXT) == []
    status = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'steer-unfinished'})
    assert status['contextPersisted'] is False
    assert ph.store.claim(tid) is None


async def test_legacy_busy_table_migrates_and_keeps_existing_input(ph):
    tid, turn, bridge = await running_turn(ph)
    await ph.call('turn/steer', steer(tid, client='legacy', expectedTurnId=turn))
    await publications(ph)
    before = tuple(ph.store.db.execute(
        'SELECT thread_id,message_id,reserved_turn_id,context_turn_id,offered,persisted FROM busy_inputs').fetchone())
    # Recreate exactly the pre-r34 schema on this private fixture database.
    ph.store.db.execute('ALTER TABLE busy_inputs DROP COLUMN stage')
    ph.server.busy_delivery = ph.engine.busy_delivery = BusyDelivery(ph.server)
    assert 'stage' in {r[1] for r in ph.store.db.execute('PRAGMA table_info(busy_inputs)')}
    assert tuple(ph.store.db.execute(
        'SELECT thread_id,message_id,reserved_turn_id,context_turn_id,offered,persisted FROM busy_inputs').fetchone()) == before
    await post_hook(ph, bridge)
    await result(ph, tid)
    assert live(ph, STAGE, 'legacy') == expected(tid, turn, 'legacy', ['registered', 'offered', 'contextPersisted'])
    assert saved(ph, tid, STAGE, 'legacy') == live(ph, STAGE, 'legacy')


async def test_terminal_callback_rejects_insert_before_any_input_side_effect_and_resets(ph, monkeypatch):
    tid = await ph.thread()
    entered, release = asyncio.Event(), asyncio.Event()
    notify = ph.engine.notify

    async def gate(thread, method, params):
        if method == CONTEXT and not entered.is_set():
            entered.set()
            await release.wait()
        await notify(thread, method, params)

    monkeypatch.setattr(ph.engine, 'notify', gate)
    turn = await ph.turn(tid, 'finish first')
    try:
        await asyncio.wait_for(entered.wait(), 5)
        owner = ph.engine.driver.owners[tid]
        assert owner.finishing_turn_id == turn and owner.active.turn_id == turn
        assert ph.store.turn(turn)['status'] == 'inProgress'
        inputs = [tuple(r) for r in ph.store.db.execute('SELECT * FROM inputs')]
        busy = [tuple(r) for r in ph.store.db.execute('SELECT * FROM busy_inputs')]
        items = ph.store.items(turn)
        rejected = await ph.call('turn/steer', steer(tid, client='finishing', expectedTurnId=turn), error=True)
        assert rejected['code'] == -32009 and rejected['data']['reason'] == 'turnFinishing'
        assert [tuple(r) for r in ph.store.db.execute('SELECT * FROM inputs')] == inputs
        assert [tuple(r) for r in ph.store.db.execute('SELECT * FROM busy_inputs')] == busy
        assert ph.store.items(turn) == items
        assert not saved(ph, tid, STAGE, 'finishing')
        assert not live(ph, STAGE, 'finishing')
        assert ph.sdk.clients[0].query_count == 1
    finally:
        release.set()
    await ph.drain()
    tid, following, bridge = await running_turn(ph, tid)
    assert following != turn and owner.finishing_turn_id is None
    await ph.call('turn/steer', steer(tid, client='after-finish', expectedTurnId=following))
    await post_hook(ph, bridge)
    await result(ph, tid)
    assert live(ph, STAGE, 'after-finish') == expected(tid, following, 'after-finish', ['registered', 'offered', 'contextPersisted'])


@pytest.mark.parametrize('release_by', ['result', 'quiet-window', 'interrupt', 'close'])
async def test_notification_hold_visible_in_event_and_read_then_clears(ph, monkeypatch, release_by):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    # hold=False lets the quiet-window case execute the foreground query once
    # the adapter releases it, without a fixture-native autonomous turn open.
    await autonomous(ph, client, owner, 'BACKGROUND', hold=release_by != 'quiet-window')
    turn = await ph.turn(tid, 'FOREGROUND')
    await until(lambda: owner.notification_held)
    statuses = lambda: [p['status'] for p in live(ph, 'thread/status/changed') if p['threadId'] == tid]
    assert statuses()[-1]['type'] == 'active'
    assert 'heldByNotification' in statuses()[-1]['activeFlags']
    wire = (await ph.call('thread/read', {'threadId': tid}))['thread']
    assert wire['status']['type'] == 'active'
    assert 'heldByNotification' in wire['status']['activeFlags']
    assert client.query_count == 0 and len(owner.deferred) == 1
    if release_by == 'result':
        await client.write({'type': 'autonomous', 'text': 'BACKGROUND-END', 'finish': True})
    elif release_by == 'quiet-window':
        # Expire the actual owner's monotonic deadline without a timing race
        # against thread/read on slow Windows CI machines.
        owner.autonomous_until = asyncio.get_running_loop().time() - 1
        # Rate-limit frames wake the owner without extending the quiet window.
        await client.events.put({'type': 'rate_limit_event'})
    elif release_by == 'interrupt':
        await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    else:
        await ph.call('claude/runtime/close', {'threadId': tid})
    await ph.drain()
    await until(lambda: not owner.notification_held)
    wire = (await ph.call('thread/read', {'threadId': tid}))['thread']
    assert 'heldByNotification' not in wire['status'].get('activeFlags', [])
    assert 'heldByNotification' not in statuses()[-1].get('activeFlags', [])
    held_at = next(i for i, status in enumerate(statuses()) if 'heldByNotification' in status.get('activeFlags', []))
    assert any('heldByNotification' not in s.get('activeFlags', []) for s in statuses()[held_at + 1:])
    assert owner.deferred == []
    assert ph.store.turn(turn)['status'] == ('interrupted' if release_by in {'interrupt', 'close'} else 'completed')
    assert client.query_count == (0 if release_by in {'interrupt', 'close'} else 1)
