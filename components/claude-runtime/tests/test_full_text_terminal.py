"""Offline fake-SDK coverage for the negotiated full-text terminal guarantee."""
import asyncio
import copy

import pytest

from cute_claude.protocol import FEATURES
from cute_claude.subscription import OutcomeUncertain
from test_reasoning import delta, item_events, result, run, start, stop, stream
from test_subscription import sh


def completed(harness, turn):
    return {item['id']: item for method, _, item in item_events(harness, turn)
            if method == 'item/completed'}


def assert_all_text_items_closed(harness, turn):
    events = item_events(harness, turn)
    started = {item['id'] for method, kind, item in events
               if method == 'item/started' and kind in {'agentMessage', 'reasoning'}}
    assert started <= completed(harness, turn).keys()
    terminal = next(i for i, event in enumerate(harness.out)
                    if event.get('method') == 'turn/completed' and event['params']['turn']['id'] == turn)
    for item_id in started:
        assert any(i < terminal and event.get('method') == 'item/completed'
                   and event.get('params', {}).get('item', {}).get('id') == item_id
                   for i, event in enumerate(harness.out))


async def test_canonical_correction_keeps_streamed_identity_before_index_reuse(sh):
    def script(sid):
        return [start(sid, 0, 'text'), delta(sid, 0, 'text_delta', text='a'), stop(sid, 0),
                {'type': 'assistant', 'content': [{'type': 'text', 'text': 'ab'}]},
                start(sid, 0, 'text'), delta(sid, 0, 'text_delta', text='c'), stop(sid, 0),
                {'type': 'assistant', 'content': [{'type': 'text', 'text': 'cd'}]}, result(sid)]
    _, turn = await run(sh, script)
    starts = [item for method, kind, item in item_events(sh, turn)
              if method == 'item/started' and kind == 'agentMessage']
    assert len(starts) == 2
    finals = completed(sh, turn)
    assert [finals[item['id']]['text'] for item in starts] == ['ab', 'cd']
    assert len([item for item in sh.store.items(turn) if item['type'] == 'agentMessage']) == 2
    assert_all_text_items_closed(sh, turn)


async def test_text_block_order_does_not_depend_on_redacted_thinking_indices_or_prose(sh):
    def script(sid):
        return [start(sid, 0, 'redacted_thinking'), stop(sid, 0),
                start(sid, 1, 'text'), delta(sid, 1, 'text_delta', text='prefix one'),
                start(sid, 2, 'text'), delta(sid, 2, 'text_delta', text='prefix two'),
                {'type': 'assistant', 'content': [{'type': 'text', 'text': '汉🙂 canonical'},
                                                  {'type': 'text', 'text': 'same final'}]}, result(sid)]
    _, turn = await run(sh, script)
    starts = [item for method, kind, item in item_events(sh, turn)
              if method == 'item/started' and kind == 'agentMessage']
    assert len(starts) == 2
    finals = completed(sh, turn)
    assert [finals[item['id']]['text'] for item in starts] == ['汉🙂 canonical', 'same final']
    assert_all_text_items_closed(sh, turn)


async def test_index_overwrite_closes_old_public_text_before_starting_replacement(sh):
    def script(sid):
        return [start(sid, 0, 'text'), delta(sid, 0, 'text_delta', text='old prefix'),
                start(sid, 0, 'text'), delta(sid, 0, 'text_delta', text='new prefix'), result(sid)]
    _, turn = await run(sh, script)
    events = item_events(sh, turn)
    starts = [item for method, kind, item in events if method == 'item/started' and kind == 'agentMessage']
    assert len(starts) == 2
    finals = completed(sh, turn)
    assert [finals[item['id']]['text'] for item in starts] == ['old prefix', 'new prefix']
    old_end = next(i for i, (method, _, item) in enumerate(events)
                   if method == 'item/completed' and item['id'] == starts[0]['id'])
    next_start = next(i for i, (method, _, item) in enumerate(events)
                      if method == 'item/started' and item['id'] == starts[1]['id'])
    assert old_end < next_start
    assert_all_text_items_closed(sh, turn)


async def test_message_start_closes_unconfirmed_blocks_before_canonical_of_next_message(sh):
    def script(sid):
        return [start(sid, 5, 'text'), delta(sid, 5, 'text_delta', text='previous message'),
                start(sid, 7, 'thinking'), delta(sid, 7, 'thinking_delta', thinking='old summary'),
                stream(sid, {'type': 'message_start', 'message': {'id': 'message-next'}}),
                start(sid, 0, 'text'), delta(sid, 0, 'text_delta', text='next prefix'),
                {'type': 'assistant', 'content': [{'type': 'text', 'text': 'next canonical'}]}, result(sid)]
    _, turn = await run(sh, script)
    starts = [item for method, kind, item in item_events(sh, turn)
              if method == 'item/started' and kind == 'agentMessage']
    finals = completed(sh, turn)
    assert [finals[item['id']]['text'] for item in starts] == ['previous message', 'next canonical']
    reasoning = [item for item in finals.values() if item['type'] == 'reasoning']
    assert len(reasoning) == 1 and reasoning[0]['summary'] == ['old summary']
    assert reasoning[0]['content'] == []
    assert_all_text_items_closed(sh, turn)


@pytest.mark.parametrize('failure', ['cancel', 'error', 'disconnect'])
async def test_abnormal_terminal_contains_complete_received_text_and_summary(sh, failure):
    async def behavior(**kw):
        sid, emit = kw['session_id'], kw['on_message']
        for event in [stream(sid, {'type': 'content_block_start', 'index': 0,
                                  'content_block': {'type': 'text', 'text': '初始'}}),
                      delta(sid, 0, 'text_delta', text='后续🙂'),
                      stream(sid, {'type': 'content_block_start', 'index': 1,
                                   'content_block': {'type': 'thinking', 'thinking': '先想'}}),
                      delta(sid, 1, 'thinking_delta', thinking='再想')]:
            await emit(event)
        if failure == 'cancel':
            raise asyncio.CancelledError()
        if failure == 'disconnect':
            raise OutcomeUncertain('synthetic transport disconnected')
        raise RuntimeError('synthetic provider failure')
    sh.provider.behavior = behavior
    tid = await sh.thread()
    turn = await sh.turn(tid, 'offline public-text terminal test')
    await sh.drain()
    assert sh.store.turn(turn)['status'] == ('interrupted' if failure == 'cancel' else 'failed')
    finals = completed(sh, turn)
    messages = [item for item in finals.values() if item['type'] == 'agentMessage']
    reasoning = [item for item in finals.values() if item['type'] == 'reasoning']
    assert len(messages) == 1 and messages[0]['text'] == '初始后续🙂'
    assert len(reasoning) == 1 and reasoning[0]['summary'] == ['先想再想']
    assert reasoning[0]['content'] == []
    assert_all_text_items_closed(sh, turn)


@pytest.mark.parametrize('feature', [True, False, None, 'true'])
async def test_full_text_terminal_initialize_root_is_exact_boolean(sh, monkeypatch, feature):
    if feature is None:
        monkeypatch.delitem(FEATURES['features'], 'fullTextItemTerminal', raising=False)
    else:
        monkeypatch.setitem(FEATURES['features'], 'fullTextItemTerminal', feature)
    out = []
    async def send(message):
        out.append(copy.deepcopy(message))
    peer = sh.server.connect(send)
    await sh.server.handle(peer, {'id': 'terminal-capability', 'method': 'initialize', 'params': {}})
    assert out[-1]['result']['fullTextItemTerminal'] is (feature is True)
    sh.server.disconnect(peer)
