"""Next Up delivery boundaries: private stores and fake SDK, never a live account."""
from test_agent_messages import message, WAKE_TEXT
from test_persistent import ph  # noqa: F401
from test_questions_plan import begin
from test_perms_steer import steer, post_hook
from test_owner_carries import carried
from test_r8_services import install, envelope, invoke
from cute_claude.external_input import semantic_digest
from cute_claude.errors import RpcError
import pytest


async def test_accepted_passive_notes_are_read_by_every_next_real_owner_turn(ph):
    tid = await ph.thread()
    for n in range(3):
        mid = f'accepted-{n}'
        await ph.call('claude/input/submit', message(tid, mid, f'NOTE-{n}'))
        assert ph.store.pending_notes(tid) == 1
        assert tid not in ph.engine.workers, 'accepting a note starts no worker'
        turn = await ph.turn(tid, f'OWNER-{n}')
        await ph.drain()
        assert ph.store.pending_notes(tid) == 0
        assert [i['id'] for i in ph.store.items(turn) if i['type'] == 'functionCallOutput'] == [mid]
        blocks = ph.sdk.clients[0].messages[-1]['message']['content']
        assert f'NOTE-{n}' in blocks[0]['text']
        assert blocks[-1]['text'] == f'OWNER-{n}'


async def test_native_accepted_notes_get_exact_receipts_on_next_real_turn(ph):
    service = install(ph, subscription=True)
    tid = await ph.thread()
    for n in range(3):
        value = envelope(tid, f'native-{n}', passive=True)
        await invoke(service, ph, 'submit', value)
        assert service._status(tid, f'native-{n}', value['semanticSha256'])['deliveryState'] == 'pending'
        turn = await ph.turn(tid, f'OWNER-{n}')
        await ph.drain()
        status = service._status(tid, f'native-{n}', value['semanticSha256'])
        assert status['receipt']['turnId'] == turn
        assert ph.store.pending_notes(tid) == 0


async def test_lost_outer_observer_notifications_do_not_stop_an_accepted_note_being_read(ph):
    service = install(ph, subscription=True)
    tid = await ph.thread()
    value = envelope(tid, 'observer-offline', passive=True)
    await invoke(service, ph, 'submit', value)
    # Keep the adapter's local receipt observer, but drop outbound notifications
    # as if the daemon's subscription disconnected or entered backoff.
    ph.engine.notify = service.observe_event
    turn = await ph.turn(tid, 'continue while outer observer is unavailable')
    await ph.drain()
    assert service._status(tid, 'observer-offline', value['semanticSha256'])['receipt']['turnId'] == turn
    assert ph.store.pending_notes(tid) == 0
    assert any(i['id'] == 'observer-offline' for i in ph.store.items(turn))


@pytest.mark.parametrize('wrong', ['runtimeGeneration', 'threadId'])
async def test_wrong_native_binding_is_rejected_before_accepting_a_note(ph, wrong):
    service = install(ph, subscription=True)
    tid = await ph.thread()
    value = envelope(tid, 'wrong-binding', passive=True)
    value[wrong] = 8 if wrong == 'runtimeGeneration' else 'other-thread'
    value['semanticSha256'] = semantic_digest(value)
    with pytest.raises(RpcError):
        await invoke(service, ph, 'submit', value)
    assert ph.store.pending_notes(tid) == 0


async def test_wake_before_bus_submission_cannot_read_a_note_not_in_the_inbox(ph):
    tid = await ph.thread()
    first = await ph.turn(tid, WAKE_TEXT)
    await ph.drain()
    assert ph.sdk.clients[0].messages[-1]['message']['content'] == WAKE_TEXT
    # The observer's Bus -> adapter delivery happens only after that turn ended.
    await ph.call('claude/input/submit', message(tid, 'delayed-bus-note', 'DELAYED'))
    assert ph.store.pending_notes(tid) == 1
    assert not any(i['id'] == 'delayed-bus-note' for i in ph.store.items(first))
    following = await ph.turn(tid, 'next actual turn')
    await ph.drain()
    assert any(i['id'] == 'delayed-bus-note' for i in ph.store.items(following))


async def test_busy_owner_can_carry_a_ready_result_in_the_same_hook(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    result = carried(tid, 'ready-result')
    await ph.call('turn/steer', steer(tid, 'OWNER', ownerCarries=[result]))
    context = (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext']
    assert 'NOTICE' in context and context.endswith('OWNER')
    assert ph.store.pending_notes(tid) == 0
    assert service._status(tid, 'ready-result', result['semanticSha256'])['receipt'] is None
    assert any(i['id'] == 'ready-result' for i in ph.store.items(turn))


async def test_failure_hook_reads_notes_only_for_an_admitted_main_chain_call(ph):
    tid, turn, bridge = await begin(ph)
    await ph.call('claude/input/submit', message(tid, 'after-failure', 'UNREAD-AFTER-FAILURE'))
    failure = ph.sdk.clients[0].options.hooks['PostToolUseFailure'][0].hooks[0]
    response = await failure({'session_id': bridge.sid, 'hook_event_name': 'PostToolUseFailure',
                              'tool_name': 'Read', 'error': 'private fixture failure'}, 'failed-tool', {})
    assert 'UNREAD-AFTER-FAILURE' not in str(response)
    assert ph.store.pending_notes(tid) == 1
    assert not any(i['id'] == 'after-failure' for i in ph.store.items(turn))
    data = {'session_id': bridge.sid, 'hook_event_name': 'PostToolUseFailure',
            'tool_name': 'Read', 'tool_input': {'file_path': str(ph.settings.workspace / 'missing.txt')}, 'error': 'missing'}
    await bridge.pre_tool(data, 'admitted-failure', {})
    await ph.call('turn/steer', steer(tid, 'OWNER-AFTER-FAILURE'))
    response = await failure(data, 'admitted-failure', {})
    assert response['hookSpecificOutput']['hookEventName'] == 'PostToolUseFailure'
    context = response['hookSpecificOutput']['additionalContext']
    assert 'UNREAD-AFTER-FAILURE' in context and 'OWNER-AFTER-FAILURE' in context
    assert ph.store.pending_notes(tid) == 0
    assert await failure(data, 'admitted-failure', {}) == {}, 'one call cannot claim twice'
