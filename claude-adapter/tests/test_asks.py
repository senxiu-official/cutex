"""cutex asks (委托): exclusive admission, ask ids, receipts."""
from __future__ import annotations

import asyncio


def ask(tid, mid='ask-msg-1', *, asks=('ask-1',), attempt=1, event='ASK', text='Please review the diff.'):
    metadata = {'cutexSessionId': 'cutex.target', 'deliveryMode': 'after-turn', 'asks': list(asks)}
    if attempt is not None:
        metadata['attempt'] = attempt
    return {'threadId': tid, 'messageId': mid, 'source': {'kind': 'agent', 'id': 'cutex.requester'},
            'eventType': event, 'text': text, 'metadata': metadata, 'admission': 'exclusive_turn'}


async def test_capabilities_offer_exclusive_admission(h):
    caps = await h.call('claude/capabilities')
    assert caps['input']['admissions'] == ['exclusive_turn']
    assert caps['input']['askEvents'] == ['ASK', 'ANSWER', 'QUESTION']
    assert caps['input']['policy'] == 'fifo-separate-turns'


async def test_exclusive_ask_starts_its_own_turn_with_ids(h):
    tid = await h.thread()
    receipt = await h.call('claude/input/submit', ask(tid))
    assert receipt['turnId'] and receipt['state'] == 'queued'
    assert receipt['ask'] == {'asks': ['ask-1'], 'attempt': 1}
    await h.drain()
    status = await h.call('claude/input/status', {'threadId': tid, 'messageId': 'ask-msg-1'})
    assert status['state'] == 'completed' and status['turnId'] == receipt['turnId']
    assert status['ask'] == {'asks': ['ask-1'], 'attempt': 1}
    prompt = h.provider.requests[0]['messages'][0]['content'][0]['text']
    assert prompt.startswith('EXTERNAL_INPUT_JSON')
    assert '"eventType":"ASK"' in prompt and '"ask":{"asks":["ask-1"],"attempt":1}' in prompt
    started = [m['params']['turn']['id'] for m in h.out if m.get('method') == 'turn/started']
    assert started == [receipt['turnId']]
    presentation = h.store.input_presentation(tid, 'ask-msg-1')
    assert presentation['presentation']['title'].startswith('委托 from cutex.requester · ask-1 · attempt 1')


async def test_exclusive_refuses_busy_without_queueing_and_replays_known_ids(h):
    tid = await h.thread()
    active = await h.turn(tid, 'sleep:0.2')
    error = await h.call('claude/input/submit', ask(tid), error=True)
    assert error['code'] == -32600
    assert error['data'] == {'externalInputError': 'busy', 'reason': 'active_turn'}
    assert not h.store.input_known(tid, 'ask-msg-1')
    await h.drain()
    first = await h.call('claude/input/submit', ask(tid))
    # A resubmit of the same id while that turn runs returns its receipt:
    # never a refusal, never a second turn.
    replay = await h.call('claude/input/submit', ask(tid))
    assert replay['duplicate'] is True and replay['turnId'] == first['turnId']
    await h.drain()
    started = [m['params']['turn']['id'] for m in h.out if m.get('method') == 'turn/started']
    assert started == [active, first['turnId']]


async def test_exclusive_refuses_closed_gate_and_plan_mode(h):
    tid = await h.thread()
    turn = await h.turn(tid, 'sleep:10')
    await asyncio.sleep(0.01)
    await h.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    error = await h.call('claude/input/submit', ask(tid), error=True)
    assert error['data']['reason'] == 'gate_closed'
    await h.call('claude/queue/resume', {'threadId': tid, 'acknowledgeUncertainEffects': True})
    h.store.db.execute('INSERT OR REPLACE INTO thread_plan_mode VALUES(?,?)', (tid, 1))
    error = await h.call('claude/input/submit', ask(tid), error=True)
    assert error['data']['reason'] == 'plan_mode'
    h.store.db.execute('INSERT OR REPLACE INTO thread_plan_mode VALUES(?,?)', (tid, 0))
    assert (await h.call('claude/input/submit', ask(tid)))['state'] == 'queued'
    await h.drain()


async def test_answer_events_and_metadata_validation(h):
    tid = await h.thread()
    params = ask(tid, 'evt-1', asks=('ask-1', 'ask-2'), attempt=None, event='ANSWER', text='## Answer\nDone.')
    params['source'] = {'kind': 'service', 'id': 'cutex-asks'}
    receipt = await h.call('claude/input/submit', params)
    assert receipt['ask'] == {'asks': ['ask-1', 'ask-2']}
    await h.drain()
    title = h.store.input_presentation(tid, 'evt-1')['presentation']['title']
    assert title.startswith('委托 Answers · ask-1, ask-2')
    bad = ask(tid, 'bad-1', attempt=0)
    assert (await h.call('claude/input/submit', bad, error=True))['code'] == -32602
    bad = {**ask(tid, 'bad-2'), 'admission': 'merge'}
    assert (await h.call('claude/input/submit', bad, error=True))['code'] == -32602
    # Plain agent messages are unchanged (no ask frame, no admission).
    plain = {'threadId': tid, 'messageId': 'm', 'source': {'kind': 'agent', 'id': 'x'}, 'eventType': 'agent_message', 'text': 'hi'}
    assert 'ask' not in await h.call('claude/input/submit', plain)
    await h.drain()
