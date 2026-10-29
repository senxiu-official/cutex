"""Private adapter proof: typed overload then a fresh bounded Continue input.

The real adapter/engine/store run against the existing fake SDK; no account,
CLI process, credential discovery or external network is used.
"""
from test_persistent import ph
from test_r8_services import install, envelope, binding
from cute_claude.external_input import semantic_digest


async def test_overload_then_new_continue_preserves_failed_turn_and_never_replays_input(ph):
    install(ph, subscription=True)
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    original_query = client.query

    async def overloaded(prompt):
        client.query_count += 1
        sid = ph.engine.session(tid)['session_id']
        await client.events.put({'type': 'assistant', 'error': 'server_error',
                                 'content': [{'type': 'text', 'text': 'HTTP 529 overloaded_error'}]})
        await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'error_during_execution',
                                 'is_error': True, 'num_turns': 1, 'usage': {}})

    client.query = overloaded
    failed = await ph.turn(tid, 'Original input must not be replayed')
    await ph.drain()
    assert ph.store.turn(failed)['error']['codexErrorInfo'] == 'serverOverloaded'
    assert client.query_count == 1
    client.query = original_query
    await ph.call('thread/externalInput/resume', binding(tid))
    value = envelope(tid, 'cutex-model-overload-private-1')
    value['message']['text'] = 'Continue.'
    value['message']['type'] = 'model_overload_continue'
    value['semanticSha256'] = semantic_digest(value)
    result = await ph.call('thread/externalInput/submit', {**value, 'admission': 'exclusive_turn'})
    continued = result['turnId']
    assert continued != failed
    await ph.drain()
    status = await ph.call('thread/externalInput/status', {**binding(tid), 'messages': [
        {'messageId': value['message']['id'], 'semanticSha256': value['semanticSha256']}]})
    assert status['statuses'][0]['deliveryState'] == 'context_persisted'
    assert status['statuses'][0]['receipt']['turnId'] == continued
    assert ph.store.turn(failed)['status'] == 'failed'
    assert ph.store.turn(continued)['status'] == 'completed'
    assert client.query_count == 2 and len(ph.sdk.clients) == 1
    # The adapter also proves stable-key idempotency if an old transport ever
    # delivers a duplicate; the new backend itself never repeats submit.
    replay = await ph.call('thread/externalInput/submit', {**value, 'admission': 'exclusive_turn'})
    await ph.drain()
    assert replay['turnId'] == continued
    assert client.query_count == 2
