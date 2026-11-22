"""One result turn may cross Stop without reopening the runtime inbox."""
import asyncio
import copy


def answer(tid, mid='cutex-ask-event-result-1'):
    return {'threadId': tid, 'messageId': mid,
            'source': {'kind': 'service', 'id': 'cutex-asks'}, 'eventType': 'ANSWER',
            'text': 'A delegation answered.', 'metadata': {'asks': ['ask-1'], 'deliveryMode': 'after-turn'},
            'view': {'schema': 'cutex.delegation-results.v1', 'data': {'stopAnswerSince': '2026-11-17T00:00:00Z'}},
            'admission': 'stop_answer'}


async def stop(h, tid):
    active = await h.turn(tid, 'sleep:10')
    await asyncio.sleep(0.01)
    await h.call('turn/interrupt', {'threadId': tid, 'turnId': active})
    assert h.store.thread(tid)['halted']
    return active


async def test_stop_answer_starts_once_and_leaves_older_owner_and_external_inputs_held(h):
    tid = await h.thread()
    stopped = await stop(h, tid)
    for mid, source in [('owner-old', 'user'), ('external-old', 'agent')]:
        h.store.enqueue(tid, mid, {'source': {'kind': source, 'id': 'old'},
                                  'eventType': 'message', 'text': 'Stay held.', 'metadata': {}}, 100)
    first = await h.call('claude/input/submit', answer(tid))
    assert first['state'] == 'processing'
    replay = await h.call('claude/input/submit', answer(tid))
    assert replay['duplicate'] and replay['turnId'] == first['turnId']
    await h.drain()
    assert h.store.thread(tid)['halted'] == 1
    assert h.store.input_status(tid, 'owner-old')['state'] == 'queued'
    assert h.store.input_status(tid, 'external-old')['state'] == 'queued'
    assert h.store.input_status(tid, first['messageId'])['state'] == 'completed'
    completed_replay = await h.call('claude/input/submit', answer(tid))
    assert completed_replay['duplicate']
    await h.drain()
    started = [m['params']['turn']['id'] for m in h.out if m.get('method') == 'turn/started']
    assert started == [stopped, first['turnId']]


async def test_scoped_store_admission_survives_reopen_and_does_not_claim_other_inputs(h):
    from cute_claude.store import Store
    tid = await h.thread()
    await stop(h, tid)
    h.store.enqueue(tid, 'owner-old', {'source': {'kind': 'user', 'id': 'old'},
                                      'eventType': 'message', 'text': 'Stay held.', 'metadata': {}}, 100)
    params = answer(tid)
    payload = {k: params[k] for k in ['source', 'eventType', 'text', 'metadata', 'view']}
    first = h.store.enqueue(tid, params['messageId'], payload, 100, stop_answer=True)
    h.store.close()
    reopened = Store(h.settings.state_dir)
    h.store = h.engine.store = h.server.store = reopened
    try:
        assert reopened.thread(tid)['halted']
        row = reopened.claim(tid)
        assert row['message_id'] == params['messageId']
        reopened.finish(tid, row['turn_id'], 'completed')
        assert reopened.claim(tid) is None
        assert reopened.input_status(tid, 'owner-old')['state'] == 'queued'
        assert reopened.enqueue(tid, params['messageId'], payload, 100, stop_answer=True)['turnId'] == first['turnId']
    finally:
        pass  # Harness owns the reopened store and closes it in teardown.


async def test_stop_answer_does_not_bypass_active_turn_plan_mode_or_tool_pause(h):
    tid = await h.thread()
    active = await h.turn(tid, 'sleep:10')
    error = await h.call('claude/input/submit', answer(tid), error=True)
    assert error['data']['reason'] == 'active_turn'
    await asyncio.sleep(0.01)
    await h.call('turn/interrupt', {'threadId': tid, 'turnId': active})
    await h.engine.tool_gate.set(tid, True)
    error = await h.call('claude/input/submit', answer(tid), error=True)
    assert error['data']['reason'] == 'agent_paused'
    await h.engine.tool_gate.set(tid, False)
    h.store.db.execute('INSERT OR REPLACE INTO thread_plan_mode VALUES(?,?)', (tid, 1))
    error = await h.call('claude/input/submit', answer(tid), error=True)
    assert error['data']['reason'] == 'plan_mode'
    h.store.db.execute('INSERT OR REPLACE INTO thread_plan_mode VALUES(?,?)', (tid, 0))
    h.store.db.execute('UPDATE threads SET archived=1 WHERE id=?', (tid,))
    error = await h.call('claude/input/submit', answer(tid), error=True)
    assert error['data']['reason'] == 'gate_closed'
    assert not h.store.input_known(tid, answer(tid)['messageId'])


async def test_stop_answer_rejects_other_sources_and_kinds_without_input_or_resume(h):
    tid = await h.thread()
    await stop(h, tid)
    for change in [{'eventType': 'QUESTION'}, {'source': {'kind': 'agent', 'id': 'pretend'}},
                   {'source': {'kind': 'user', 'id': 'owner'}}, {'messageId': 'arbitrary'},
                   {'view': None}, {'metadata': {'asks': [], 'deliveryMode': 'after-turn'}}]:
        params = copy.deepcopy(answer(tid)); params.update(change)
        await h.call('claude/input/submit', params, error=True)
        assert h.store.thread(tid)['halted']
        assert not h.store.input_known(tid, params['messageId'])
