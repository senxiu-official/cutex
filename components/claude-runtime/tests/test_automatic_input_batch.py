"""Atomic publication, claim order and receipts for an automatic batch."""
from types import SimpleNamespace

import pytest

from cute_claude.errors import RpcError
from cute_claude.external_input import semantic_digest
from test_busy_delivery import hook_for
from test_context_receipts import append, attachment, parent
from test_persistent import ph  # noqa: F401
from test_questions_plan import begin
from test_r8_services import binding, envelope, install, invoke


def batch(tid, *, early):
    values = [envelope(tid, mid, passive=mid != 'result') for mid in ('before', 'result', 'after')]
    if early:
        values[1]['message']['delivery'] = 'soon'
    for value in values:
        value['message']['text'] = 'frame-' + value['message']['id']
        value['semanticSha256'] = semantic_digest(value)
    return {**binding(tid), 'triggerId': 'result', 'messages': values}


def status(service, tid, payload):
    return [service._status(tid, value['message']['id'], value['semanticSha256']) for value in payload['messages']]


def assert_order(prompt):
    assert prompt.index('frame-before') < prompt.index('frame-result') < prompt.index('frame-after')


def test_ordered_batch_capability_requires_a_bound_subscription(ph):
    from dataclasses import replace

    assert 'externalInputOrderedBatch' not in ph.server.external_inputs.capabilities()
    service = install(ph, subscription=True)
    assert service.capabilities()['externalInputOrderedBatch'] == 1
    service.settings = replace(service.settings, backend='messages')
    assert service.enabled and 'externalInputOrderedBatch' not in service.capabilities()


async def test_automatic_batch_claims_once_at_an_authenticated_gap(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    payload = batch(tid, early=True)
    response = await invoke(service, ph, 'submitBatch', payload, after=False)
    assert response['statuses'] == status(service, tid, payload)
    assert all(row['receipt'] is None for row in response['statuses'])
    assert ph.store.pending_notes(tid) == 0
    proof_root = ph.engine.driver.config_dir / 'projects' / 'batch-proof'
    proof_root.mkdir(parents=True)
    proof = SimpleNamespace(sid=bridge.sid, path=proof_root / f'{bridge.sid}.jsonl')
    proof.path.write_bytes(b'')

    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'unknown-tool', {}) == {}
    context = (await hook(data, 'tool-1', {}))['hookSpecificOutput']['additionalContext']
    assert_order(context)
    assert await hook(data, 'tool-1', {}) == {}
    callback = parent(proof)
    append(proof, callback)
    observer = ph.server.busy_delivery.context_receipts
    await observer.flush(tid, turn)
    assert all(row['receipt'] is None for row in status(service, tid, payload))
    append(proof, attachment(proof, callback, context=context))
    await observer.flush(tid, turn)
    receipts = (await invoke(service, ph, 'submitBatch', payload, after=False))['statuses']
    assert all(row['receipt']['turnId'] == turn for row in receipts)
    assert [row['receipt']['ordinal'] for row in receipts] == [1, 2, 3]
    ph.store.finish(tid, turn, 'completed')
    assert ph.store.claim(tid) is None


async def test_automatic_batch_without_a_gap_waits_for_the_following_turn(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    payload = batch(tid, early=True)
    await invoke(service, ph, 'submitBatch', payload, after=False)
    ph.store.finish(tid, turn, 'completed')
    assert all(row['receipt'] is None for row in status(service, tid, payload))
    following = ph.store.claim(tid)
    assert following['turn_id'] != turn
    assert_order(following['prompt'])
    assert [item['id'] for item in ph.store.items(following['turn_id'])] == ['before', 'result', 'after']
    assert all(row['receipt'] is None for row in status(service, tid, payload))


async def test_note_batch_waits_for_an_independent_start_without_waking(ph):
    service = install(ph, subscription=True)
    tid = await ph.thread()
    payload = {**binding(tid), 'messages': [envelope(tid, 'first', passive=True),
                                          envelope(tid, 'second', passive=True)]}
    await invoke(service, ph, 'submitBatch', payload)
    assert not ph.engine.workers and ph.store.claim(tid) is None

    ph.store.enqueue(tid, 'owner', {'source': {'kind': 'user', 'id': 'owner'},
                                    'eventType': 'user.message', 'text': 'work'}, 10)
    started = ph.store.claim(tid)
    assert [item['id'] for item in ph.store.items(started['turn_id'])][1:] == ['first', 'second']
    assert all(row['receipt'] is None for row in status(service, tid, payload))


async def test_automatic_notes_later_than_the_final_answer_wait_for_next_turn(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    ph.store.put_item(turn, {'id': 'final', 'type': 'agentMessage', 'phase': 'final_answer', 'text': 'done'})
    payload = {**binding(tid), 'messages': [envelope(tid, 'late', passive=True)]}
    await invoke(service, ph, 'submitBatch', payload, after=False)
    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'tool-1', {}) == {}
    assert service.settle_notes(tid, turn) == ([], [])
    assert status(service, tid, payload)[0]['receipt'] is None
    assert ph.store.pending_notes(tid) == 1


async def test_idle_note_publication_cannot_join_a_turn_that_started_in_flight(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    payload = {**binding(tid), 'notesAtStart': True, 'messages': [envelope(tid, 'idle-note', passive=True)]}
    await invoke(service, ph, 'submitBatch', payload, after=False)
    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'tool-1', {}) == {}
    with pytest.raises(RpcError, match='previously submitted identity'):
        await invoke(service, ph, 'submitBatch', {**payload, 'notesAtStart': False}, after=False)
    ph.store.finish(tid, turn, 'completed')
    ph.store.enqueue(tid, 'next', {'source': {'kind': 'user', 'id': 'owner'},
                                   'eventType': 'user.message', 'text': 'next'}, 10)
    following = ph.store.claim(tid)
    assert [item['id'] for item in ph.store.items(following['turn_id'])][1:] == ['idle-note']


async def test_later_trigger_can_take_older_start_bound_notes_in_order(ph):
    tid = await ph.thread()
    service = install(ph, subscription=True)
    await invoke(service, ph, 'submitBatch', {
        **binding(tid), 'notesAtStart': True,
        'messages': [envelope(tid, 'waited-idle', passive=True)],
    }, after=False)
    assert ph.store.claim(tid) is None
    await invoke(service, ph, 'submitBatch', {
        **binding(tid), 'triggerId': 'result',
        'messages': [envelope(tid, 'result'),
                     envelope(tid, 'newer-note', passive=True)],
    }, after=False)
    turn = ph.store.claim(tid)
    assert [item['id'] for item in ph.store.items(turn['turn_id'])] == ['waited-idle', 'result', 'newer-note']


async def test_cumulative_passive_capacity_preserves_the_next_trigger_and_replay(ph, monkeypatch):
    from cute_claude import external_input, store

    tid = await ph.thread()
    service = install(ph, subscription=True)
    monkeypatch.setattr(external_input, 'MAX_PASSIVE_MEMBERS', 3)
    note = lambda name: envelope(tid, name, passive=True)
    first = {**binding(tid), 'notesAtStart': True, 'messages': [note('first'), note('second')]}
    second = {**binding(tid), 'notesAtStart': True, 'messages': [note('third')]}
    before = await invoke(service, ph, 'batchCapacity', binding(tid))
    accepted = await invoke(service, ph, 'submitBatch', first, after=False)
    await invoke(service, ph, 'submitBatch', second, after=False)
    used = sum(len(store.dump(value).encode()) for value in first['messages'] + second['messages'])
    full = await invoke(service, ph, 'batchCapacity', binding(tid))
    assert full['availableMembers'] == 0
    assert before['availableBytes'] - full['availableBytes'] == used
    assert ph.store.claim(tid) is None
    assert await invoke(service, ph, 'submitBatch', first, after=False) == accepted

    over = {**binding(tid), 'triggerId': 'result',
            'messages': [envelope(tid, 'result'), note('overflow')]}
    with pytest.raises(RpcError, match='automatic_passive_capacity'):
        await invoke(service, ph, 'submitBatch', over, after=False)
    assert all(row['deliveryState'] == 'unknown' for row in status(service, tid, over))
    await invoke(service, ph, 'submitBatch', {**over, 'messages': over['messages'][:1]}, after=False)
    turn = ph.store.claim(tid)
    assert [item['id'] for item in ph.store.items(turn['turn_id'])] == ['first', 'second', 'third', 'result']
    assert (await invoke(service, ph, 'batchCapacity', binding(tid)))['availableMembers'] == 3
    assert all(row['receipt'] is None for row in status(service, tid, first))
    with pytest.raises(RpcError, match='binding mismatch'):
        await invoke(service, ph, 'batchCapacity', {**binding(tid), 'runtimeGeneration': 8})


async def test_cumulative_passive_capacity_counts_bytes_until_the_notes_are_claimed(ph, monkeypatch):
    from cute_claude import external_input, store

    tid = await ph.thread()
    service = install(ph, subscription=True)
    older = envelope(tid, 'larger-first', passive=True)
    later = envelope(tid, 'later', passive=True)
    bound = len(store.dump(older).encode())
    monkeypatch.setattr(external_input, 'MAX_BATCH_BYTES', bound)
    first = {**binding(tid), 'notesAtStart': True, 'messages': [older]}
    second = {**binding(tid), 'notesAtStart': True, 'messages': [later]}
    await invoke(service, ph, 'submitBatch', first, after=False)
    free = await invoke(service, ph, 'batchCapacity', binding(tid))
    assert free['availableMembers'] > 0 and free['availableBytes'] == 0
    with pytest.raises(RpcError, match='automatic_passive_capacity'):
        await invoke(service, ph, 'submitBatch', second, after=False)
    ph.store.enqueue(tid, 'owner', {'source': {'kind': 'user', 'id': 'owner'},
                                   'eventType': 'user.message', 'text': 'start'}, 10)
    turn = ph.store.claim(tid)
    assert ph.store.items(turn['turn_id'])[-1]['id'] == 'larger-first'
    await invoke(service, ph, 'submitBatch', second, after=False)


async def test_note_batch_waits_for_exact_callback_evidence(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    payload = {**binding(tid), 'messages': [envelope(tid, 'first', passive=True),
                                          envelope(tid, 'second', passive=True)]}
    await invoke(service, ph, 'submitBatch', payload, after=False)
    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'unknown-tool', {}) == {}
    assert ph.store.pending_notes(tid) == 2
    assert (await hook(data, 'tool-1', {}))['hookSpecificOutput']['additionalContext']
    assert ph.store.pending_notes(tid) == 0
    assert all(row['receipt'] is None for row in status(service, tid, payload))
    await ph.server.busy_delivery.acknowledge_messages(tid, turn, ['first'])
    receipts = status(service, tid, payload)
    assert receipts[0]['receipt']['turnId'] == turn
    assert receipts[1]['receipt'] is None


async def test_automatic_batch_rejects_a_reordered_replay_and_rolls_back_capacity_failure(ph):
    from dataclasses import replace

    tid, _, _ = await begin(ph)
    service = install(ph, subscription=True)
    payload = batch(tid, early=True)
    service.settings = replace(service.settings, max_pending=2)
    # Keep one note in the inbox so insertion fails after reserving the trigger
    # and first companion.
    await invoke(service, ph, 'submit', envelope(tid, 'existing', passive=True), after=False)
    with pytest.raises(RpcError, match='inbox capacity reached'):
        await invoke(service, ph, 'submitBatch', payload, after=False)
    assert all(row['deliveryState'] == 'unknown' for row in status(service, tid, payload))
    service.settings = replace(service.settings, max_pending=10)
    await invoke(service, ph, 'submitBatch', payload, after=False)
    with pytest.raises(RpcError, match='batch order'):
        await invoke(service, ph, 'submitBatch', {**payload, 'messages': list(reversed(payload['messages']))}, after=False)


async def test_ordinary_submit_cannot_set_internal_batch_metadata(ph):
    tid = await ph.thread()
    result = await ph.call('claude/input/submit', {'threadId': tid, 'messageId': 'legacy',
        'source': {'kind': 'agent', 'id': 'sender'}, 'text': 'legacy note',
        'metadata': {'deliveryMode': 'passive', 'automaticNoteBatch': 'forged'}}, error=True)
    assert result['message'] == 'automatic batch metadata is reserved'
    assert ph.store.pending_notes(tid) == 0
