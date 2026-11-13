"""057 owner carriers: fake SDK only, no account or live daemon."""
import asyncio
import hashlib
from dataclasses import replace

import pytest

from cute_claude.busy_delivery import BusyDelivery
from cute_claude.errors import RpcError
from cute_claude.external_input import ExternalInputService, semantic_digest
from cute_claude.store import dump
from test_persistent import ph, until  # noqa: F401
from test_questions_plan import begin
from test_perms_steer import steer, post_hook, PNG
from test_r8_services import envelope, install, invoke
from test_autonomous_turns import started, autonomous
from test_r35_context_timing import held, proof, write  # noqa: F401


def carried(tid, mid='notice'):
    value = envelope(tid, mid, version=2, view={'schema': 'cutex.notice.v1', 'data': {'originTurnId': 'origin', 'status': 'stopped'}})
    value['message']['text'] = 'NOTICE @not-an-owner-path'
    value['semanticSha256'] = semantic_digest(value)
    return value


def owner(tid, values, client='owner-1', value='OWNER @hello.txt'):
    return {'threadId': tid, 'input': [{'type': 'text', 'text': value}],
            'clientUserMessageId': client, 'ownerCarries': values}


async def test_idle_blocks_display_view_and_native_terminal_receipt(ph):
    service = install(ph, subscription=True)
    tid = await ph.thread()
    value = carried(tid)
    result = await ph.call('turn/start', owner(tid, [value]))
    turn = result['turn']['id']
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt'] is None
    await ph.drain()
    blocks = ph.sdk.clients[0].messages[0]['message']['content']
    assert blocks[-1]['text'] == 'OWNER @hello.txt'
    assert 'NOTICE @not-an-owner-path' in blocks[0]['text']
    items = ph.store.items(turn)
    assert items[0]['content'][0]['text'] == 'OWNER @hello.txt'
    item = next(i for i in items if i['id'] == 'notice')
    assert item['externalInputView'] == value['view']
    assert 'service-id' in item['output']
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt']['turnId'] == turn
    assert ph.server.external_inputs.capabilities()['ownerInputCarries'] == 1


async def test_busy_atomic_hook_and_exact_receipt_selection(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    values = [carried(tid, 'one'), carried(tid, 'two')]
    params = steer(tid, 'OWNER', ownerCarries=values)
    await ph.call('turn/steer', params)
    assert ph.store.take_notes(tid, turn) == [], 'ordinary note hook cannot steal reserved carries'
    await ph.call('turn/steer', params)
    context = (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext']
    assert context.count('NOTICE @not-an-owner-path') == 2 and context.endswith('OWNER')
    assert ph.store.take_notes(tid, turn) == []
    assert (await post_hook(ph, bridge, 'second')) == {}
    for value in values:
        assert service._status(tid, value['message']['id'], value['semanticSha256'])['receipt'] is None
    await ph.server.busy_delivery.acknowledge_messages(tid, turn, ['one'])
    assert service._status(tid, 'one', values[0]['semanticSha256'])['receipt']['turnId'] == turn
    assert service._status(tid, 'two', values[1]['semanticSha256'])['receipt'] is None
    await ph.server.busy_delivery.acknowledge_messages(tid, turn, ['two'])
    assert service._status(tid, 'two', values[1]['semanticSha256'])['receipt']['turnId'] == turn
    ph.store.finish(tid, turn, 'completed')
    assert ph.store.claim(tid) is None


async def test_capacity_refusal_rolls_back_entire_owner_and_notes(ph):
    install(ph, subscription=True)
    ph.server.settings = replace(ph.server.settings, max_pending=1)
    ph.server.external_inputs.settings = ph.server.settings
    tid = await ph.thread()
    err = await ph.call('turn/start', owner(tid, [carried(tid, 'one'), carried(tid, 'two')]), error=True)
    assert err['code'] == -32001
    for table in ('inputs', 'notes', 'external_inputs', 'turns', 'items'):
        assert ph.store.db.execute(f'SELECT count(*) FROM {table}').fetchone()[0] == 0


@pytest.mark.parametrize('bad', ['digest', 'owner', 'thread', 'generation', 'view', 'size', 'duplicate', 'fields'])
async def test_invalid_envelopes_are_atomic(ph, bad):
    install(ph, subscription=True)
    tid = await ph.thread()
    value = carried(tid)
    values = [value]
    if bad == 'digest': value['semanticSha256'] = '0' * 64
    elif bad == 'owner': value['ownerId'] = 'other'
    elif bad == 'thread': value['threadId'] = await ph.thread()
    elif bad == 'generation': value['runtimeGeneration'] = 8
    elif bad == 'view': value['view']['data']['float'] = 1.2
    elif bad == 'size': value['message']['text'] = 'x' * 65537
    elif bad == 'duplicate': values.append(value)
    elif bad == 'fields': value['admission'] = 'queue'
    assert (await ph.call('turn/start', owner(tid, values), error=True))['code'] < 0
    assert ph.store.db.execute('SELECT count(*) FROM inputs').fetchone()[0] == 0
    assert ph.store.db.execute('SELECT count(*) FROM notes').fetchone()[0] == 0


async def test_duplicate_owner_start_is_stable_and_conflicting_retry_rejected(ph):
    install(ph, subscription=True)
    tid = await ph.thread()
    params = owner(tid, [carried(tid)])
    first, _ = await ph.server.dispatch(ph.peer, 'turn/start', params)
    again, _ = await ph.server.dispatch(ph.peer, 'turn/start', params)
    assert again['turn']['id'] == first['turn']['id']
    assert ph.store.db.execute('SELECT count(*) FROM turns').fetchone()[0] == 1
    with pytest.raises(RpcError):
        await ph.server.dispatch(ph.peer, 'turn/start', {**params, 'input': [{'type': 'text', 'text': 'changed'}]})


async def test_busy_fallback_keeps_carried_items_with_owner(ph):
    tid, first, bridge = await begin(ph)
    install(ph, subscription=True)
    await ph.call('turn/steer', steer(tid, 'OWNER', ownerCarries=[carried(tid)]))
    ph.store.finish(tid, first, 'completed')
    following = ph.store.claim(tid)
    assert following['message_id'] == 'steer-c-1'
    assert 'NOTICE' in ph.store.turn_note_frames(following['turn_id'])[0]
    assert ph.store.items(following['turn_id'])[0]['content'][0]['text'] == 'OWNER'


@pytest.mark.parametrize('offered', [False, True])
async def test_interrupt_and_restart_never_replay_ambiguous_carriers(ph, offered):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = carried(tid)
    params = steer(tid, 'OWNER', ownerCarries=[value])
    await ph.call('turn/steer', params)
    if offered:
        await post_hook(ph, bridge)
        ph.store.put_item(turn, {'type': 'agentMessage', 'id': 'output', 'text': 'not persistence proof'})
    ph.store.drop_unread_steers(tid)
    await bridge.end_turn()
    ph.store.finish(tid, turn, 'interrupted')
    await service.observe_event(tid, 'turn/completed', {'turn': {'id': turn, 'status': 'interrupted'}})
    BusyDelivery(ph.server)
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt'] is None
    await ph.turn(tid, 'NEW OWNER')
    await ph.drain()
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt'] is None
    assert ph.store.take_notes(tid, turn) == []
    assert 'NOTICE' not in dump(ph.sdk.clients[0].messages)


async def test_autonomous_result_cannot_ack_or_answer_owner_carrier(ph, monkeypatch):
    import cute_claude.sdk_runtime as runtime
    monkeypatch.setattr(runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    service = install(ph, subscription=True)
    tid, client, runtime_owner = await started(ph)
    await autonomous(ph, client, runtime_owner, 'BACKGROUND-START')
    value = carried(tid)
    result = await ph.call('turn/start', owner(tid, [value], value='FOREGROUND'))
    turn = result['turn']['id']
    await until(lambda: len(runtime_owner.deferred) == 1)
    assert client.query_count == 0
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt'] is None
    await client.write({'type': 'autonomous', 'text': 'BACKGROUND-END', 'finish': True})
    await ph.drain()
    assert client.query_count == 1
    assert 'BACKGROUND' not in dump(ph.store.items(turn))
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt']['turnId'] == turn


async def test_native_transcript_proves_exact_carried_group_before_result(held):
    ph, tid, turn, bridge, path = held
    service = ph.server.external_inputs
    first = carried(tid, 'first')
    await ph.call('turn/steer', steer(tid, 'OWNER FIRST', client='first', ownerCarries=[first]))
    context = (await post_hook(ph, bridge, 'read-first'))['hookSpecificOutput']['additionalContext']
    second = carried(tid, 'second')
    await ph.call('turn/steer', steer(tid, 'OWNER SECOND', client='second', ownerCarries=[second]))
    await post_hook(ph, bridge, 'read-second')
    parent, attachment = proof(bridge.sid, 'read-first', context)
    write(path, parent)
    await ph.server.busy_delivery.context_receipts.flush(tid, turn)
    assert service._status(tid, 'first', first['semanticSha256'])['receipt'] is None
    write(path, attachment)
    await ph.server.busy_delivery.context_receipts.flush(tid, turn)
    await until(lambda: service._status(tid, 'first', first['semanticSha256'])['receipt'] is not None)
    assert service._status(tid, 'first', first['semanticSha256'])['receipt']['turnId'] == turn
    assert service._status(tid, 'second', second['semanticSha256'])['receipt'] is None
    assert ph.store.turn(turn)['status'] == 'inProgress'


async def test_finishing_busy_refusal_does_not_reserve_any_carries(ph):
    tid, turn, bridge = await begin(ph)
    install(ph, subscription=True)
    ph.engine.driver.owners[tid].finishing_turn_id = turn
    err = await ph.call('turn/steer', steer(tid, ownerCarries=[carried(tid)]), error=True)
    assert err['data']['reason'] == 'turnFinishing'
    assert ph.store.db.execute('SELECT count(*) FROM notes').fetchone()[0] == 0
    assert ph.store.db.execute('SELECT count(*) FROM external_inputs').fetchone()[0] == 0


async def test_initialize_exposes_numeric_capability_at_top_level(ph):
    install(ph, subscription=True)
    peer = ph.server.connect(ph.send)
    initialized, _ = await ph.server.dispatch(peer, 'initialize', {})
    assert type(initialized['ownerInputCarries']) is int and initialized['ownerInputCarries'] == 1
    caps = await ph.call('claude/capabilities')
    assert type(caps['ownerInputCarries']) is int and caps['ownerInputCarries'] == 1


async def test_conflict_after_first_reservation_rolls_back_without_touching_existing(ph):
    service = install(ph, subscription=True)
    tid = await ph.thread()
    existing = carried(tid, 'existing')
    existing['message']['delivery'] = 'passive'
    existing['semanticSha256'] = semantic_digest(existing)
    await service.dispatch(ph.peer, 'thread/externalInput/submit', existing)
    original = ph.store.db.execute("SELECT * FROM notes WHERE message_id='existing'").fetchone()
    # The first new carry is reserved before the second identity conflicts;
    # rollback must retain only the independent original note.
    with pytest.raises(RpcError):
        await ph.server.dispatch(ph.peer, 'turn/start', owner(tid, [carried(tid, 'new'), existing]))
    assert ph.store.db.execute('SELECT count(*) FROM notes').fetchone()[0] == 1
    assert ph.store.db.execute('SELECT count(*) FROM inputs').fetchone()[0] == 0
    assert ph.store.db.execute('SELECT count(*) FROM turns').fetchone()[0] == 0
    assert ph.store.db.execute('SELECT count(*) FROM external_inputs').fetchone()[0] == 1
    assert dict(ph.store.db.execute("SELECT * FROM notes WHERE message_id='existing'").fetchone()) == dict(original)


LONG_CLIENT_IDS = ['x' * 201, 'x' * 256, '界' * 85, '🙂' * 64]


@pytest.mark.parametrize('client_id', LONG_CLIENT_IDS)
async def test_long_owner_client_id_keeps_full_display_and_stable_carrier(ph, client_id):
    service = install(ph, subscription=True)
    tid = await ph.thread()
    value = carried(tid)
    params = owner(tid, [value], client=client_id)
    turn = (await ph.call('turn/start', params))['turn']['id']
    await ph.drain()
    again = await ph.call('turn/start', params)
    await ph.drain()
    assert again['turn']['id'] == turn
    key = 'owner-' + hashlib.sha256(client_id.encode('utf-8')).hexdigest()
    row = ph.store.db.execute('SELECT message_id,payload FROM inputs WHERE turn_id=?', (turn,)).fetchone()
    assert row['message_id'] == key and len(key.encode('utf-8')) <= 256
    assert ph.store.items(turn)[0]['clientId'] == client_id
    assert ph.store.db.execute('SELECT count(*) FROM turns').fetchone()[0] == 1
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt']['turnId'] == turn


@pytest.mark.parametrize('client_id', LONG_CLIENT_IDS)
async def test_long_busy_client_id_echo_and_duplicate_are_exact(ph, client_id):
    tid, turn, bridge = await begin(ph)
    install(ph, subscription=True)
    params = steer(tid, 'OWNER', client=client_id, ownerCarries=[carried(tid)])
    await ph.call('turn/steer', params)
    await ph.call('turn/steer', params)
    context = (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext']
    assert context.count('NOTICE @not-an-owner-path') == 1 and context.endswith('OWNER')
    key = 'steer-' + hashlib.sha256(client_id.encode('utf-8')).hexdigest()
    item = next(i for i in ph.store.items(turn) if i['id'] == key)
    assert item['clientId'] == client_id and item['steered'] is True
    assert ph.server.busy_delivery.client_id(tid, key) == client_id
    assert ph.store.db.execute("SELECT count(*) FROM inputs WHERE message_id=?", (key,)).fetchone()[0] == 1


async def test_overflow_ids_with_the_same_200_byte_prefix_are_distinct(ph):
    tid, turn, bridge = await begin(ph)
    install(ph, subscription=True)
    ids = ['x' * 200 + suffix for suffix in ('first', 'second')]
    for index, client_id in enumerate(ids):
        await ph.call('turn/steer', steer(tid, client_id, client=client_id,
                      ownerCarries=[carried(tid, 'notice-' + str(index))]))
    await post_hook(ph, bridge)
    items = [i for i in ph.store.items(turn) if i.get('steered')]
    assert [i['clientId'] for i in items] == ids
    assert len({i['id'] for i in items}) == 2


@pytest.mark.parametrize('prefix', ['owner-', 'steer-'])
def test_carrier_key_preserves_short_ids_and_uses_utf8_byte_boundary(prefix):
    from cute_claude.protocol import owner_carrier_key
    for client_id in ['short', 'x' * 200, '界' * 66]:
        assert owner_carrier_key(prefix, client_id) == prefix + client_id
    for client_id in LONG_CLIENT_IDS:
        assert owner_carrier_key(prefix, client_id) == prefix + hashlib.sha256(client_id.encode('utf-8')).hexdigest()
    with pytest.raises(RpcError):
        owner_carrier_key(prefix, 'x' * 257)
    with pytest.raises(RpcError):
        owner_carrier_key(prefix, '界' * 86)


@pytest.mark.parametrize('busy_fallback', [False, True])
async def test_image_only_carrier_keeps_external_text_verbatim(ph, busy_fallback):
    if busy_fallback:
        tid, prior, bridge = await begin(ph)
    else:
        tid = await ph.thread()
    service = install(ph, subscription=True)
    value = carried(tid)
    client_id = '🙂' * 64
    image = {'type': 'image', 'url': 'data:image/png;base64,' + PNG}
    params = {'threadId': tid, 'clientUserMessageId': client_id,
              'input': [image], 'ownerCarries': [value]}
    if busy_fallback:
        # Image steers cannot use a text hook. Preserve the whole group for
        # its FIFO owner turn, with no independent passive-note submission.
        await ph.server.dispatch(ph.peer, 'turn/steer', params)
        assert ph.store.take_notes(tid, prior) == []
        await bridge.end_turn()
        ph.store.finish(tid, prior, 'completed')
        record = ph.store.claim(tid)
        turn = record['turn_id']
        assert record['prompt'] == ''
        ph.engine.wake(tid)
    else:
        turn = (await ph.call('turn/start', params))['turn']['id']
    expected_notes = ph.store.turn_note_frames(turn)
    assert len(expected_notes) == 1 and '@not-an-owner-path' in expected_notes[0]
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt'] is None
    await ph.drain()
    message = ph.sdk.clients[0].messages[-1]
    assert message.get('client_composed') is True, 'no external block may be treated as typed owner input'
    blocks = message['message']['content']
    assert [b['text'] for b in blocks if b['type'] == 'text'] == expected_notes
    assert blocks[-1] == {'type': 'image', 'source': {'type': 'base64', 'media_type': 'image/png', 'data': PNG}}
    shown = ph.store.items(turn)[0]
    assert shown['type'] == 'userMessage' and shown['clientId'] == client_id
    assert shown['content'] == [image], 'image-only display must not gain external or invented owner text'
    assert service._status(tid, 'notice', value['semanticSha256'])['receipt']['turnId'] == turn


def carry_key(value):
    return {'messageId': value['message']['id'], 'semanticSha256': value['semanticSha256']}


async def take_back(ph, service, values):
    binding = {key: values[0][key] for key in ('ownerId', 'threadId', 'runtimeGeneration')}
    result, _ = await service.dispatch(ph.peer, 'thread/externalInput/returnCarries',
                                       {**binding, 'version': 1, 'messages': [carry_key(value) for value in values]})
    return result['returned']


async def test_a_dropped_steer_returns_its_unread_notes_once_and_they_can_go_out_again(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    assert service.capabilities()['ownerCarryReturn'] == 1
    value = carried(tid)
    await ph.call('turn/steer', steer(tid, 'OWNER', ownerCarries=[value]))
    assert await take_back(ph, service, [value]) == [], 'its steer can still read it'
    ph.store.drop_unread_steers(tid)
    await bridge.end_turn()
    ph.store.finish(tid, turn, 'interrupted')
    assert await take_back(ph, service, [value]) == ['notice']
    # A lost answer: asking again gets the same answer, nothing else changes.
    assert await take_back(ph, service, [value]) == ['notice']
    assert ph.store.db.execute('SELECT count(*) FROM notes').fetchone()[0] == 0
    status = service._status(tid, 'notice', value['semanticSha256'])
    assert status['deliveryState'] == 'unknown' and status['processing']['state'] == 'none'
    # cutex submits it on its own now, under the same identity: exactly one copy.
    independent = {**value, 'message': {**value['message'], 'delivery': 'passive'}}
    independent['semanticSha256'] = semantic_digest(independent)
    await service.dispatch(ph.peer, 'thread/externalInput/submit', independent)
    assert ph.store.db.execute("SELECT count(*) FROM notes WHERE message_id='notice'").fetchone()[0] == 1
    assert await take_back(ph, service, [independent]) == [], 'no longer carried'
    assert await take_back(ph, service, [value]) == [], 'the old key is no proof any more'
    following = await ph.turn(tid, 'NEXT')
    await ph.drain()
    assert dump(ph.sdk.clients[0].messages).count('NOTICE @not-an-owner-path') == 1
    assert service._status(tid, 'notice', independent['semanticSha256'])['receipt']['turnId'] == following


@pytest.mark.parametrize('case', ['offered', 'completed', 'wrong-digest', 'claimed-as-turn'])
async def test_a_note_not_proven_unread_stays_where_it_is(ph, case):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = carried(tid)
    await ph.call('turn/steer', steer(tid, 'OWNER', ownerCarries=[value]))
    asked = value
    if case == 'offered':
        await post_hook(ph, bridge)
        ph.store.drop_unread_steers(tid)
        await bridge.end_turn()
        ph.store.finish(tid, turn, 'interrupted')
    elif case == 'completed':
        # The steer waits for the next turn: it can still read the note.
        await bridge.end_turn()
        ph.store.finish(tid, turn, 'completed')
    elif case == 'wrong-digest':
        ph.store.drop_unread_steers(tid)
        asked = {**value, 'semanticSha256': '0' * 64}
    elif case == 'claimed-as-turn':
        # Its own turn read the note, and that turn was then interrupted.
        await bridge.end_turn()
        ph.store.finish(tid, turn, 'completed')
        claimed = ph.store.claim(tid)
        ph.store.finish(tid, claimed['turn_id'], 'interrupted')
    before = [dict(row) for row in ph.store.db.execute('SELECT * FROM notes')]
    assert await take_back(ph, service, [asked]) == []
    assert [dict(row) for row in ph.store.db.execute('SELECT * FROM notes')] == before
    assert service._row(tid, 'notice') is not None


async def test_a_restored_steer_cannot_read_a_note_already_returned(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = carried(tid)
    await ph.call('turn/steer', steer(tid, 'OWNER', ownerCarries=[value]))
    dropped = ph.store.drop_unread_steers(tid)
    assert await take_back(ph, service, [value]) == ['notice']
    # The interrupt failed: the steer waits for its turn again, without the note.
    ph.store.restore_steers(tid, dropped)
    context = (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext']
    assert context.endswith('OWNER') and 'NOTICE' not in context


async def test_the_returned_proof_survives_a_restart(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = carried(tid)
    await ph.call('turn/steer', steer(tid, 'OWNER', ownerCarries=[value]))
    ph.store.drop_unread_steers(tid)
    assert await take_back(ph, service, [value]) == ['notice']
    await bridge.end_turn()
    ph.store.finish(tid, turn, 'interrupted')
    again = ExternalInputService(ph.server)
    assert again._return_carries(tid, [carry_key(value)]) == ['notice']


async def test_resending_a_dropped_steer_alone_runs_it_once_and_returns_its_notes(ph):
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    value = carried(tid)
    params = steer(tid, 'OWNER', ownerCarries=[value])
    await ph.call('turn/steer', params)
    ph.store.drop_unread_steers(tid)
    alone = {key: item for key, item in params.items() if key != 'ownerCarries'}
    await ph.call('turn/steer', alone)
    await ph.call('turn/steer', alone)
    assert await take_back(ph, service, [value]) == ['notice'], 'the resend took it back'
    context = (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext']
    assert context.count('OWNER') == 1 and 'NOTICE' not in context
    assert ph.store.db.execute("SELECT count(*) FROM inputs WHERE message_id='steer-c-1'").fetchone()[0] == 1


async def test_a_note_this_runtime_never_received_is_not_returned(ph):
    # A cutex crash after its reservation, before any native request.
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    assert await take_back(ph, service, [carried(tid, 'never-sent')]) == []


@pytest.mark.parametrize('case', ['read', 'still-queued'])
async def test_a_steer_not_proven_dropped_is_never_run_again(ph, case):
    tid, turn, bridge = await begin(ph)
    install(ph, subscription=True)
    params = steer(tid, 'OWNER')
    await ph.call('turn/steer', params)
    if case == 'read':
        await post_hook(ph, bridge)
        ph.store.drop_unread_steers(tid)
    await ph.call('turn/steer', params)
    rows = ph.store.db.execute("SELECT state FROM inputs WHERE message_id='steer-c-1'").fetchall()
    assert len(rows) == 1 and rows[0][0] == ('injecting' if case == 'read' else 'queued')
    if case == 'read':
        assert (await post_hook(ph, bridge, 'second')) == {}


async def test_explicit_retry_uses_fresh_native_id_when_old_carry_stays_reserved(ph):
    """Reproduce native dedup, then the daemon's stable-source/new-attempt retry."""
    tid, turn, bridge = await begin(ph)
    service = install(ph, subscription=True)
    old = carried(tid)
    await ph.call('turn/steer', steer(tid, 'OWNER', ownerCarries=[old]))
    ph.store.drop_unread_steers(tid)
    await bridge.end_turn()
    ph.store.finish(tid, turn, 'interrupted')
    # No returnCarries handshake: native still holds this id. Repeating the
    # original submit cannot turn its reserved carry into an independent input.
    await invoke(service, ph, 'submit', old)
    following = await ph.turn(tid, 'NEXT')
    await ph.drain()
    assert 'NOTICE @not-an-owner-path' not in dump(ph.sdk.clients[0].messages)
    assert service._status(tid, 'notice', old['semanticSha256'])['receipt'] is None
    # Same source content and view; a fresh daemon attempt bypasses that id.
    retried = {**old, 'message': {**old['message'], 'id': 'bus-retry:11111111111141118111111111111111:notice'}}
    retried['semanticSha256'] = semantic_digest(retried)
    await invoke(service, ph, 'submit', retried)
    await ph.drain()
    await until(lambda: service._status(tid, retried['message']['id'], retried['semanticSha256'])['receipt'] is not None)
    assert dump(ph.sdk.clients[0].messages).count('NOTICE @not-an-owner-path') == 1
    assert service._status(tid, 'notice', old['semanticSha256'])['receipt'] is None
