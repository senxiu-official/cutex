"""Authenticated hook delivery, receipt fencing and FIFO/turn-boundary races."""
import json
import time

import pytest

from cute_claude.busy_delivery import BusyDelivery
from cute_claude.external_input import semantic_digest
from test_persistent import ph  # noqa: F401
from test_questions_plan import begin
from test_r8_services import envelope, install, invoke


def soon(tid, mid='wake'):
    value = envelope(tid, mid)
    value['message']['delivery'] = 'soon'
    value['semanticSha256'] = semantic_digest(value)
    return value


def result(tid, mid='cutex-ask-soon-result'):
    return {'threadId': tid, 'messageId': mid, 'source': {'kind': 'service', 'id': 'cutex-asks'},
            'eventType': 'ANSWER', 'text': 'delegation result', 'admission': 'next_step',
            'metadata': {'deliveryMode': 'soon', 'asks': ['ask-1']},
            'view': {'schema': 'cutex.delegation-results.v1', 'data': {'items': []}}}


async def hook_for(ph, bridge, call='tool-1'):
    data = {'session_id': bridge.sid, 'hook_event_name': 'PostToolUse',
            'tool_name': 'Read', 'tool_input': {'file_path': str(ph.settings.workspace / 'hello.txt')},
            'tool_response': 'ok'}
    await bridge.pre_tool(data, call, {})
    return ph.sdk.clients[0].options.hooks['PostToolUse'][0].hooks[0], data


async def test_native_soon_claims_once_and_waits_for_sdk_context_ack(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = soon(tid)
    assert (await invoke(service, ph, 'submit', value, after=False))['statuses'][0]['deliveryState'] == 'pending'
    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'unknown-tool', {}) == {}, 'unadmitted callback cannot consume a wake'
    assert await hook({**data, 'agent_id': 'child'}, 'tool-1', {}) == {}
    assert await hook({**data, 'session_id': 'wrong'}, 'tool-1', {}) == {}
    context = (await hook(data, 'tool-1', {}))['hookSpecificOutput']['additionalContext']
    assert context.startswith('EXTERNAL_INPUT_JSON') and 'hello' in context
    assert await hook(data, 'tool-1', {}) == {}, 'duplicate hook cannot offer the input twice'
    status = service._status(tid, 'wake', value['semanticSha256'])
    assert status['deliveryState'] == 'pending' and status['processing']['state'] == 'claimed'
    await service.observe_event(tid, 'item/completed', {'threadId': tid, 'turnId': turn, 'item': {'type': 'agentMessage'}})
    assert service._status(tid, 'wake', value['semanticSha256'])['deliveryState'] == 'pending'
    await ph.server.busy_delivery.acknowledge(tid, turn)
    receipt = service._status(tid, 'wake', value['semanticSha256'])['receipt']
    assert receipt['turnId'] == turn
    ph.store.finish(tid, turn, 'completed')
    assert ph.store.claim(tid) is None, 'consumed wake never starts another FIFO turn'
    assert (await invoke(service, ph, 'submit', value))['statuses'][0]['receipt'] == receipt
    assert len(ph.store.db.execute('SELECT * FROM turns').fetchall()) == 1


async def test_no_tool_or_turn_boundary_leaves_exactly_one_fifo_turn(ph):
    tid, first, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = soon(tid)
    await invoke(service, ph, 'submit', value, after=False)
    ph.store.finish(tid, first, 'completed')
    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'tool-1', {}) == {}, 'late callback cannot claim into completed turn'
    following = ph.store.claim(tid)
    assert following['message_id'] == 'wake' and following['turn_id'] != first
    await ph.engine.emit(tid, 'claude/input/contextPersisted',
                         {'threadId': tid, 'turnId': following['turn_id'], 'messageId': 'wake'})
    ph.store.finish(tid, following['turn_id'], 'completed')
    assert ph.store.claim(tid) is None
    assert service._status(tid, 'wake', value['semanticSha256'])['receipt']['turnId'] == following['turn_id']


async def test_hook_crash_and_reopened_service_never_replay_unknown_input(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = soon(tid)
    await invoke(service, ph, 'submit', value, after=False)
    hook, data = await hook_for(ph, bridge)
    await hook(data, 'tool-1', {})
    BusyDelivery(ph.server)  # restart recovery: offered context is outcome unknown
    status = service._status(tid, 'wake', value['semanticSha256'])
    assert status['deliveryState'] == 'pending' and status['processing']['reason'] == 'request_uncertain'
    assert (await invoke(service, ph, 'submit', value))['statuses'][0] == status
    ph.store.finish(tid, turn, 'failed')
    ph.store.db.execute('UPDATE threads SET halted=0 WHERE id=?', (tid,))
    assert ph.store.claim(tid) is None


async def test_result_metadata_and_receipt_survive_consumption_without_new_turn(ph):
    tid, turn, bridge = await begin(ph)
    submitted = await ph.call('claude/input/submit', result(tid))
    assert submitted['contextPersisted'] is False and submitted['ask'] == {'asks': ['ask-1']}
    hook, data = await hook_for(ph, bridge)
    context = (await hook(data, 'tool-1', {}))['hookSpecificOutput']['additionalContext']
    assert json.loads(context.split('\n', 1)[1])['ask'] == {'asks': ['ask-1']}
    pending = await ph.call('claude/input/status', {'threadId': tid, 'messageId': result(tid)['messageId']})
    assert pending['contextTurnId'] == turn and pending['contextPersisted'] is False
    await ph.server.busy_delivery.acknowledge(tid, turn)
    ph.store.finish(tid, turn, 'completed')
    replay = await ph.call('claude/input/submit', result(tid))
    assert replay['duplicate'] and replay['contextPersisted'] and replay['contextTurnId'] == turn
    assert replay['turnId'] == submitted['turnId']
    assert ph.store.claim(tid) is None


@pytest.mark.parametrize('mutation', [
    {'source': {'kind': 'user', 'id': 'owner'}}, {'eventType': 'ASK'},
    {'admission': 'exclusive_turn'}, {'messageId': 'wrong-id'},
    {'metadata': {'deliveryMode': 'soon', 'asks': [], 'attempt': 1}},
])
async def test_next_step_cannot_bypass_delegation_admission(ph, mutation):
    tid, _, _ = await begin(ph)
    error = await ph.call('claude/input/submit', {**result(tid), **mutation}, error=True)
    assert error['code'] < 0
    assert ph.store.db.execute('SELECT count(*) FROM busy_inputs').fetchone()[0] == 0


async def test_busy_result_rejects_plan_and_closed_gate(ph):
    tid, _, _ = await begin(ph)
    ph.store.db.execute('UPDATE threads SET halted=1 WHERE id=?', (tid,))
    error = await ph.call('claude/input/submit', result(tid), error=True)
    assert error['data']['reason'] == 'gate_closed'


async def test_mock_does_not_advertise_or_accept_soon(h):
    tid = await h.thread()
    service = install(h)
    assert 'soon' not in service.capabilities()['externalInputDeliveries']
    with pytest.raises(Exception, match='unsupported external delivery'):
        await invoke(service, h, 'submit', soon(tid))


async def test_duplicate_tool_callback_cannot_take_a_later_wake(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'tool-1', {}) == {}
    await invoke(service, ph, 'submit', soon(tid), after=False)
    assert await hook(data, 'tool-1', {}) == {}
    await bridge.pre_tool(data, 'tool-2', {})
    assert 'additionalContext' in (await hook(data, 'tool-2', {}))['hookSpecificOutput']


async def test_turn_end_during_tool_publication_leaves_one_unclaimed_fallback(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = soon(tid)
    await invoke(service, ph, 'submit', value, after=False)
    notify = ph.engine.notify

    async def terminal_race(thread, method, params):
        if method == 'item/completed':
            bridge.accepting = False
        await notify(thread, method, params)

    ph.engine.notify = terminal_race
    hook, data = await hook_for(ph, bridge)
    assert await hook(data, 'tool-1', {}) == {}
    await ph.server.busy_delivery.acknowledge(tid, turn)
    await ph.engine.emit(tid, 'claude/input/contextPersisted', {'threadId': tid, 'turnId': turn, 'messageId': 'owner'})
    assert service._status(tid, 'wake', value['semanticSha256'])['receipt'] is None
    ph.store.finish(tid, turn, 'completed')
    await ph.engine.emit(tid, 'turn/completed', {'threadId': tid, 'turn': ph.store.turn(turn)})
    following = ph.store.claim(tid)
    assert following['message_id'] == 'wake' and following['turn_id'] != turn
    ph.store.finish(tid, following['turn_id'], 'completed')
    assert ph.store.claim(tid) is None


async def test_consumed_wake_retention_and_history_revert_preserve_identity(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = soon(tid)
    await invoke(service, ph, 'submit', value, after=False)
    hook, data = await hook_for(ph, bridge)
    await hook(data, 'tool-1', {})
    await ph.server.busy_delivery.acknowledge(tid, turn)
    ph.store.finish(tid, turn, 'completed')
    await ph.engine.emit(tid, 'turn/completed', {'threadId': tid, 'turn': ph.store.turn(turn)})
    before = service._status(tid, 'wake', value['semanticSha256'])
    ph.store.db.execute('UPDATE turns SET completed=? WHERE id=?', (time.time() - 31 * 86400, turn))
    assert service.compact_history() == 1
    assert service._status(tid, 'wake', value['semanticSha256']) == before
    ph.store.db.execute('UPDATE turns SET reverted=1 WHERE id=?', (turn,))
    status = service._status(tid, 'wake', value['semanticSha256'])
    assert status['receipt'] is None and status['processing']['reason'] == 'context_missing'
    local = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'wake'})
    assert local['contextPersisted'] is False and local['state'] == 'reverted'


@pytest.mark.parametrize('owner_carry', [False, True])
async def test_retired_daemon_batch_can_coexist_with_later_native_input(ph, owner_carry):
    # The daemon may retire its old result reservation after owner dismissal.
    # That does not withdraw a native input already accepted by this runtime.
    from test_owner_carries import carried, owner
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    old = carried(tid, 'old-retired-batch') if owner_carry else soon(tid, 'old-retired-batch')
    if owner_carry:
        await ph.call('turn/steer', owner(tid, [old], client='old-carrier'))
    else:
        await invoke(service, ph, 'submit', old, after=False)
    later = soon(tid, 'later-result-batch')
    later['message']['text'] = 'LATER RESULT UNIQUE'
    later['semanticSha256'] = semantic_digest(later)
    await invoke(service, ph, 'submit', later, after=False)
    # Duplicate submission never creates a second registration of that id.
    await invoke(service, ph, 'submit', later, after=False)
    rows = ph.store.db.execute('SELECT message_id FROM busy_inputs WHERE thread_id=?', (tid,)).fetchall()
    assert {r[0] for r in rows} == ({'steer-old-carrier', 'later-result-batch'} if owner_carry
                                   else {'old-retired-batch', 'later-result-batch'})
    assert len(rows) == 2
    hook, data = await hook_for(ph, bridge)
    context = (await hook(data, 'tool-1', {}))['hookSpecificOutput']['additionalContext']
    assert 'LATER RESULT UNIQUE' in context
    assert ('NOTICE @not-an-owner-path' if owner_carry else 'hello') in context
    assert await hook(data, 'tool-1', {}) == {}
    # An old receipt must not accidentally acknowledge the later id.
    await ph.server.busy_delivery.acknowledge_messages(tid, turn, ['old-retired-batch'])
    old_status = service._status(tid, 'old-retired-batch', old['semanticSha256'])
    assert old_status['receipt']['messageId'] == 'old-retired-batch'
    assert old_status['receipt']['semanticSha256'] == old['semanticSha256']
    assert service._status(tid, 'later-result-batch', later['semanticSha256'])['receipt'] is None
    await ph.server.busy_delivery.acknowledge_messages(tid, turn, ['later-result-batch'])
    later_status = service._status(tid, 'later-result-batch', later['semanticSha256'])
    assert later_status['receipt']['messageId'] == 'later-result-batch'
    assert later_status['receipt']['turnId'] == turn
    assert later_status['receipt']['semanticSha256'] == later['semanticSha256']
    ph.store.finish(tid, turn, 'completed')
    assert ph.store.claim(tid) is None


@pytest.mark.parametrize('busy', [False, True])
async def test_alarm_input_runs_idle_or_enters_busy_tool_gap_once(ph, busy):
    if busy:
        tid, turn, bridge = await begin(ph)
    else:
        tid = await ph.thread()
    service = install(ph, subscription=True)
    value = envelope(tid, 'alarm-policy-reminder')
    value['message'].update({'source': {'kind': 'service', 'id': 'Cutex Alarm'},
                             'type': 'alarm', 'delivery': 'soon' if busy else 'after_turn',
                             'text': 'ALARM POLICY MARKER'})
    value['semanticSha256'] = semantic_digest(value)
    await invoke(service, ph, 'submit', value, after=not busy)
    if busy:
        hook, data = await hook_for(ph, bridge)
        context = (await hook(data, 'tool-1', {}))['hookSpecificOutput']['additionalContext']
        assert 'ALARM POLICY MARKER' in context
        assert await hook(data, 'tool-1', {}) == {}
        await ph.server.busy_delivery.acknowledge_messages(tid, turn, ['alarm-policy-reminder'])
        ph.store.finish(tid, turn, 'completed')
    else:
        await ph.drain()
        turns = ph.store.db.execute('SELECT id FROM turns WHERE thread_id=?', (tid,)).fetchall()
        assert len(turns) == 1, 'an idle alarm starts one clean turn'
        turn = turns[0][0]
        assert len(ph.sdk.clients[0].messages) == 1
    receipt = service._status(tid, value['message']['id'], value['semanticSha256'])['receipt']
    assert receipt['turnId'] == turn and receipt['messageId'] == value['message']['id']
    assert receipt['semanticSha256'] == value['semanticSha256']
    assert ph.store.claim(tid) is None
