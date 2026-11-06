"""claude-r35: a busy `soon` input's timeline item is published at injection.

r34 published the `external_event` item of a claimed wake / notice / delegation
result only at the successful terminal Result (`acknowledge`). r35 publishes it
right after the claiming hook returns, like an owner steer's `userMessage`.
The receipt (`contextPersisted`) keeps its own boundary and never precedes the
item. Only the SDK boundary is a fixture (test_persistent / fake_sdk).
"""
import asyncio

from test_busy_delivery import result as delegation_result, soon
from test_perms_steer import post_hook, steer
from test_persistent import ph, until  # noqa: F401 (shared integrated fixture)
from test_r34_insert_stages import live, publications, result
from test_r35_context_timing import held  # noqa: F401 (running turn, owner/generation bound)
from test_r8_services import invoke

CONTEXT = 'claude/input/contextPersisted'


def item_events(ph, method, mid=None):
    return [m['params'] for m in ph.out if m.get('method') == method
            and (mid is None or m['params']['item'].get('id') == mid)]


def position(ph, predicate):
    return next(index for index, message in enumerate(ph.out) if predicate(message))


def is_item(method, mid):
    return lambda m: m.get('method') == method and m['params']['item'].get('id') == mid


def is_receipt(mid):
    return lambda m: m.get('method') == CONTEXT and m['params'].get('messageId') == mid


async def submit_wake(ph, tid, mid='wake'):
    value = soon(tid, mid)
    submitted = await invoke(ph.server.external_inputs, ph, 'submit', value, after=False)
    assert submitted['statuses'][0]['deliveryState'] == 'pending'
    return value


async def published(ph, tid):
    async with asyncio.timeout(5):
        await ph.server.busy_delivery.items_published(tid)
    await publications(ph)


async def test_soon_wake_item_is_published_at_injection_and_not_again_at_result(held):
    ph, tid, turn, bridge, _ = held
    await submit_wake(ph, tid)
    context = (await post_hook(ph, bridge, 'read-wake'))['hookSpecificOutput']['additionalContext']
    assert context.startswith('EXTERNAL_INPUT_JSON')
    await published(ph, tid)
    started, completed = item_events(ph, 'item/started', 'wake'), item_events(ph, 'item/completed', 'wake')
    assert len(started) == len(completed) == 1, 'published once, at the claiming hook'
    for params in (started[0], completed[0]):
        assert params['threadId'] == tid and params['turnId'] == turn
        assert params['item']['type'] == 'functionCallOutput' and params['item']['name'] == 'external_event'
    assert ph.store.turn(turn)['status'] == 'inProgress', 'item precedes the terminal Result'
    assert not [p for p in live(ph, CONTEXT) if p['messageId'] == 'wake'], 'the item is not a receipt'
    status = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'wake'})
    assert status['contextTurnId'] == turn and status['contextPersisted'] is False

    await result(ph, tid)
    assert ph.store.turn(turn)['status'] == 'completed'
    assert [p['messageId'] for p in live(ph, CONTEXT)].count('wake') == 1
    assert len(item_events(ph, 'item/started', 'wake')) == len(item_events(ph, 'item/completed', 'wake')) == 1, \
        'acknowledge never publishes the item a second time'
    assert position(ph, is_item('item/completed', 'wake')) < position(ph, is_receipt('wake'))
    status = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'wake'})
    assert status['contextPersisted'] is True and status['contextTurnId'] == turn


async def test_steer_and_wake_claimed_together_publish_once_in_claim_order(held):
    ph, tid, turn, bridge, _ = held
    await submit_wake(ph, tid)
    await ph.call('turn/steer', steer(tid, client='c-1', expectedTurnId=turn))
    await post_hook(ph, bridge, 'read-both')
    await published(ph, tid)
    order = [p['item']['id'] for p in item_events(ph, 'item/started') if p['item']['id'] in ('wake', 'steer-c-1')]
    assert order == ['wake', 'steer-c-1']
    await result(ph, tid)
    order = [p['item']['id'] for p in item_events(ph, 'item/completed') if p['item']['id'] in ('wake', 'steer-c-1')]
    assert order == ['wake', 'steer-c-1'], 'no item republished at the terminal Result'
    receipts = [p['messageId'] for p in live(ph, CONTEXT)]
    assert receipts.count('wake') == 1 and receipts.count('steer-c-1') == 1


async def test_receipt_waits_for_its_pending_item_publication(held, monkeypatch):
    ph, tid, turn, bridge, _ = held
    delivery = ph.server.busy_delivery
    entered, release = asyncio.Event(), asyncio.Event()
    publish = delivery._publish

    async def blocked(thread, turn_id, item):
        entered.set()
        await release.wait()
        await publish(thread, turn_id, item)

    monkeypatch.setattr(delivery, '_publish', blocked)
    await submit_wake(ph, tid)
    await post_hook(ph, bridge, 'read-wake')
    await asyncio.wait_for(entered.wait(), 5)
    # An early native receipt (claude-r35 context_receipts) for this exact message.
    acknowledging = asyncio.create_task(delivery.acknowledge_messages(tid, turn, ('wake',)))
    for _ in range(20):
        await asyncio.sleep(0)
    assert not acknowledging.done()
    assert not [p for p in live(ph, CONTEXT) if p['messageId'] == 'wake']
    release.set()
    await asyncio.wait_for(acknowledging, 5)
    assert position(ph, is_item('item/completed', 'wake')) < position(ph, is_receipt('wake'))
    assert len(item_events(ph, 'item/completed', 'wake')) == 1


async def test_failed_turn_keeps_the_published_item_and_gives_no_receipt(held):
    ph, tid, turn, bridge, _ = held
    await submit_wake(ph, tid)
    await post_hook(ph, bridge, 'read-wake')
    await published(ph, tid)
    assert len(item_events(ph, 'item/completed', 'wake')) == 1
    await result(ph, tid, failed=True)
    assert ph.store.turn(turn)['status'] != 'completed'
    assert not [p for p in live(ph, CONTEXT) if p['messageId'] == 'wake']
    assert len(item_events(ph, 'item/completed', 'wake')) == 1
    status = await ph.call('claude/input/status', {'threadId': tid, 'messageId': 'wake'})
    assert status['contextPersisted'] is False


async def test_soon_delegation_result_item_is_published_at_injection(held):
    ph, tid, turn, bridge, _ = held
    submitted = await ph.call('claude/input/submit', delegation_result(tid))
    assert submitted['contextPersisted'] is False
    mid = delegation_result(tid)['messageId']
    await post_hook(ph, bridge, 'read-result')
    await published(ph, tid)
    completed = item_events(ph, 'item/completed', mid)
    assert len(completed) == 1 and completed[0]['turnId'] == turn
    assert completed[0]['item']['externalInputView']['schema'] == 'cutex.delegation-results.v1'
    assert ph.store.turn(turn)['status'] == 'inProgress'
    await result(ph, tid)
    assert len(item_events(ph, 'item/completed', mid)) == 1
    assert [p['messageId'] for p in live(ph, CONTEXT)].count(mid) == 1
    status = await ph.call('claude/input/status', {'threadId': tid, 'messageId': mid})
    assert status['contextPersisted'] is True and status['contextTurnId'] == turn
