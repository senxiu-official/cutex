"""Original owner IDs in one atomic turn/start; fake SDK, never live."""
import pytest
from test_persistent import ph  # noqa: F401
from test_r8_services import install
from test_perms_steer import PNG


def entry(mid, text=None):
    return {'clientUserMessageId': mid, 'input': [{'type': 'text', 'text': text or mid}]}


def batch(tid):
    return {'threadId': tid, **entry('c'), 'ownerInputs': [entry('a'), entry('b')]}


async def test_busy_batch_is_definitely_not_submitted_and_legacy_wire_is_unchanged(ph):
    from test_questions_plan import begin
    tid, _, _ = await begin(ph)
    install(ph, subscription=True)
    before = [ph.store.db.execute(f'SELECT count(*) FROM {table}').fetchone()[0]
              for table in ('inputs', 'turns', 'items')]
    error = await ph.call('turn/start', batch(tid), error=True)
    assert error['data']['ownerInputBatchNotSubmitted'] == 'not_idle'
    assert not error['data'].get('outcomeUnknown')
    legacy = await ph.call('turn/start', {'threadId': tid, **entry('legacy')}, error=True)
    assert 'ownerInputBatchNotSubmitted' not in legacy['data']
    assert before == [ph.store.db.execute(f'SELECT count(*) FROM {table}').fetchone()[0]
                      for table in ('inputs', 'turns', 'items')]


async def test_batch_claim_refusal_rolls_back_before_definite_not_submitted(ph, monkeypatch):
    install(ph, subscription=True)
    tid = await ph.thread()
    monkeypatch.setattr(ph.store, 'claim', lambda *_: None)
    error = await ph.call('turn/start', batch(tid), error=True)
    assert error['data']['ownerInputBatchNotSubmitted'] == 'pending_trigger_turn'
    assert not error['data'].get('outcomeUnknown')
    for table in ('inputs', 'turns', 'items'):
        assert ph.store.db.execute(f'SELECT count(*) FROM {table}').fetchone()[0] == 0


async def test_original_user_items_and_model_order_are_atomic_and_replay_does_not_start_twice(ph):
    service = install(ph, subscription=True)
    assert service.capabilities()['ownerInputBatch'] == 1
    tid = await ph.thread()
    params = batch(tid)
    first = await ph.call('turn/start', params)
    turn = first['turn']['id']
    await ph.drain()
    items = [i for i in ph.store.items(turn) if i['type'] == 'userMessage']
    assert [i['clientId'] for i in items] == ['a', 'b', 'c']
    content = ph.sdk.clients[0].messages[0]['message']['content']
    assert [b['text'] for b in content] == ['a', 'b', 'c']
    again = await ph.call('turn/start', params)
    assert again['turn']['id'] == turn
    await ph.drain()
    assert ph.sdk.clients[0].query_count == 1
    conflicting = {**params, 'ownerInputs': [entry('a', 'changed'), entry('b')]}
    assert (await ph.call('turn/start', conflicting, error=True))['code'] == -32010
    other = {**params, **entry('d')}
    assert (await ph.call('turn/start', other, error=True))['code'] == -32010


@pytest.mark.parametrize('bad', ['duplicate', 'carrier', 'shape', 'size'])
async def test_bad_batch_has_no_input_turn_or_partial_history(ph, bad):
    install(ph, subscription=True)
    tid = await ph.thread()
    params = batch(tid)
    if bad == 'duplicate': params['ownerInputs'] = [entry('a'), entry('a')]
    if bad == 'carrier': params['ownerInputs'] = [entry('c')]
    if bad == 'shape': params['ownerInputs'] = [{'clientUserMessageId': 'a', 'input': 'bad'}]
    if bad == 'size': params['ownerInputs'] = [entry('a', 'x' * (1024 * 1024))]
    await ph.call('turn/start', params, error=True)
    for table in ['inputs', 'turns', 'items']:
        assert ph.store.db.execute(f'SELECT count(*) FROM {table}').fetchone()[0] == 0


async def test_prefix_images_are_preserved_as_user_parts(ph):
    install(ph, subscription=True)
    tid = await ph.thread()
    image = {'type': 'image', 'url': 'data:image/png;base64,' + PNG}
    params = batch(tid)
    params['ownerInputs'][0]['input'].append(image)
    result = await ph.call('turn/start', params)
    await ph.drain()
    items = [i for i in ph.store.items(result['turn']['id']) if i['type'] == 'userMessage']
    assert items[0]['clientId'] == 'a'
    assert any(part['type'] == 'image' for part in items[0]['content'])
    content = ph.sdk.clients[0].messages[0]['message']['content']
    assert [part['type'] for part in content] == ['text', 'image', 'text', 'text']


@pytest.mark.parametrize('image_carrier', [False, True])
async def test_quota_and_notes_precede_every_owner_prefix(ph, monkeypatch, image_carrier):
    install(ph, subscription=True)
    tid = await ph.thread()
    monkeypatch.setattr(ph.engine.busy_delivery, 'continue_quota_notes', lambda *_: ['quota'])
    monkeypatch.setattr(ph.store, 'turn_note_frames', lambda *_: ['note'])
    params = batch(tid)
    if image_carrier:
        params['input'] = [{'type': 'image', 'url': 'data:image/png;base64,' + PNG}]
    await ph.call('turn/start', params)
    await ph.drain()
    message = ph.sdk.clients[0].messages[0]
    blocks = message['message']['content']
    assert [part['text'] for part in blocks if part['type'] == 'text'] == (
        ['quota', 'note', 'a', 'b'] if image_carrier else ['quota', 'note', 'a', 'b', 'c'])
    if image_carrier:
        assert blocks[-1]['type'] == 'image' and message['client_composed'] is True


@pytest.mark.parametrize('carrier', [False, True])
@pytest.mark.parametrize('characters', [64, 65])
async def test_owner_ids_are_bounded_in_utf8_bytes(ph, carrier, characters):
    install(ph, subscription=True)
    tid = await ph.thread()
    params = batch(tid)
    mid = '🙂' * characters
    if carrier:
        params['clientUserMessageId'] = mid
    else:
        params['ownerInputs'][0]['clientUserMessageId'] = mid
    if characters == 65:
        await ph.call('turn/start', params, error=True)
        for table in ['inputs', 'turns', 'items']:
            assert ph.store.db.execute(f'SELECT count(*) FROM {table}').fetchone()[0] == 0
    else:
        result = await ph.call('turn/start', params)
        await ph.drain()
        assert mid in [item['clientId'] for item in ph.store.items(result['turn']['id'])
                       if item['type'] == 'userMessage']


@pytest.mark.parametrize('image_carrier', [False, True])
async def test_slash_paths_and_command_looking_owner_text_remain_literal(ph, image_carrier):
    install(ph, subscription=True)
    tid = await ph.thread()
    params = batch(tid)
    params['ownerInputs'] = [entry('a', '/compact is literal owner text'), entry('b', '/home/owner/file @literal')]
    params['input'] = ([{'type': 'image', 'url': 'data:image/png;base64,' + PNG}]
                       if image_carrier else [{'type': 'text', 'text': '/plan is also literal'}])
    result = await ph.call('turn/start', params)
    await ph.drain()
    message = ph.sdk.clients[0].messages[0]
    assert message['client_composed'] is True
    assert [part['text'] for part in message['message']['content'] if part['type'] == 'text'][:2] == [
        '/compact is literal owner text', '/home/owner/file @literal']
    assert [i['clientId'] for i in ph.store.items(result['turn']['id']) if i['type'] == 'userMessage'] == ['a', 'b', 'c']
    assert ph.sdk.clients[0].query_count == 1
