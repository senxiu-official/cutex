"""cutex agent messaging v2 (0.4.0): passive notes, views, external_event items."""
from __future__ import annotations

import json

from test_persistent import ph  # noqa: F401 (fixture)

VIEW = {'schema': 'cutex.agent-message.v1',
        'data': {'senderId': 'cutex.lead', 'senderName': 'Lead', 'deliveryMode': 'passive',
                 'deliveryKind': 'note', 'senderRuntime': 'codex', 'body': 'log is in /tmp/run.log'}}


def message(tid, mid, text, *, mode='passive', view=VIEW):
    params = {'threadId': tid, 'messageId': mid, 'source': {'kind': 'agent', 'id': 'cutex.lead'},
              'eventType': 'agent_message', 'text': text,
              'metadata': {'sourceName': 'Lead', 'cutexSessionId': 'cutex.me', 'deliveryMode': mode}}
    if view is not None:
        params['view'] = view
    return params


def turns_started(h):
    return [m['params']['turn']['id'] for m in h.out if m.get('method') == 'turn/started']


def completed_items(h):
    return [m['params'] for m in h.out if m.get('method') == 'item/completed']


async def test_capabilities_advertise_views_and_passive_notes(h):
    caps = await h.call('claude/capabilities')
    assert caps['input']['views'] is True
    assert caps['input']['deliveryModes'] == ['after-turn', 'passive']
    assert caps['input']['itemProjection'] is True


async def test_note_starts_no_turn_and_is_read_at_the_next_agent_turn(h):
    tid = await h.thread()
    note = await h.call('claude/input/submit', message(tid, 'note-1', 'Message Type: MESSAGE\nSender: Lead (cutex.lead)\nPayload:\nlog is in /tmp/run.log'))
    assert note['turnId'] is None and note['state'] == 'queued' and note['deliveryMode'] == 'passive'
    await h.drain()
    assert turns_started(h) == [], 'a note never starts a turn'
    # Exact replay returns the same receipt; other content is a conflict.
    replay = await h.call('claude/input/submit', message(tid, 'note-1', 'Message Type: MESSAGE\nSender: Lead (cutex.lead)\nPayload:\nlog is in /tmp/run.log'))
    assert replay['duplicate'] is True and replay['turnId'] is None
    conflict = await h.call('claude/input/submit', message(tid, 'note-1', 'other'), error=True)
    assert conflict['code'] == -32010
    # A wake opens a turn: the pending note is read first, then the wake.
    wake_view = {'schema': 'cutex.agent-message.v1', 'data': {**VIEW['data'], 'deliveryMode': 'after_turn', 'deliveryKind': 'wake', 'body': 'please look'}}
    wake = await h.call('claude/input/submit', message(tid, 'wake-1', 'Message Type: MESSAGE\nSender: Lead (cutex.lead)\nPayload:\nplease look', mode='after-turn', view=wake_view))
    assert wake['turnId']
    await h.drain()
    assert turns_started(h) == [wake['turnId']]
    prompt = h.provider.requests[0]['messages'][0]['content'][0]['text']
    assert prompt.index('log is in /tmp/run.log') < prompt.index('please look')
    assert prompt.startswith('EXTERNAL_INPUT_JSON')
    items = [p for p in completed_items(h) if p['item']['type'] == 'functionCallOutput']
    assert [p['item']['id'] for p in items] == ['note-1', 'wake-1'], 'item id is the bus message id'
    assert all(p['turnId'] == wake['turnId'] for p in items)
    first = items[0]['item']
    assert first['name'] == 'external_event' and first['namespace'] == 'external'
    assert first['externalInputView'] == VIEW
    output = json.loads(first['output'])
    assert output == {'source': {'kind': 'agent', 'id': 'cutex.lead'}, 'type': 'message',
                      'text': 'Message Type: MESSAGE\nSender: Lead (cutex.lead)\nPayload:\nlog is in /tmp/run.log'}
    status = await h.call('claude/input/status', {'threadId': tid, 'messageId': 'note-1'})
    assert status['state'] == 'injected' and status['turnId'] is None, 'receipt stays stable'
    # Items survive thread/read (timeline) too.
    turn = h.store.turn(wake['turnId'])
    assert [i['id'] for i in turn['items'] if i['type'] == 'functionCallOutput'] == ['note-1', 'wake-1']


async def test_owner_turn_reads_notes_at_its_start_and_bad_views_are_refused(h):
    tid = await h.thread()
    await h.call('claude/input/submit', message(tid, 'note-2', 'fyi'))
    turn = await h.turn(tid, 'hello')
    await h.drain()
    assert turn in turns_started(h)
    # Like Codex (passive input joins a turn's first request), an owner turn reads
    # pending notes at its start, not only after its first tool call.
    assert h.store.pending_notes(tid) == 0
    prompt = h.provider.requests[0]['messages'][0]['content'][0]['text']
    assert prompt.startswith('EXTERNAL_INPUT_JSON') and prompt.index('fyi') < prompt.index('hello')
    assert [i['type'] for i in h.store.items(turn)][:2] == ['userMessage', 'functionCallOutput']
    assert [p['item']['id'] for p in completed_items(h) if p['item']['type'] == 'functionCallOutput'] == ['note-2']
    assert h.store.take_notes(tid, turn) == [], 'at most once'
    bad = message(tid, 'note-3', 'x', view={'schema': 'x', 'data': {}, 'extra': 1})
    assert (await h.call('claude/input/submit', bad, error=True))['code'] < 0
    huge = message(tid, 'note-4', 'x', view={'schema': 'x', 'data': {'body': 'y' * 17000}})
    assert (await h.call('claude/input/submit', huge, error=True))['code'] < 0
    owner_note = message(tid, 'note-5', 'x')
    owner_note['source'] = {'kind': 'user', 'id': 'me'}
    owner_note.pop('view')
    assert (await h.call('claude/input/submit', owner_note, error=True))['code'] < 0


async def test_post_tool_use_hook_reads_notes_into_the_running_turn(ph):
    from test_questions_plan import begin
    tid, turn, bridge = await begin(ph)
    hook = ph.sdk.clients[0].options.hooks['PostToolUse'][0].hooks[0]
    data = {'session_id': bridge.sid, 'hook_event_name': 'PostToolUse', 'tool_response': 'ok'}
    assert await hook(data, 'call-0', {}) == {}, 'nothing pending: plain result'
    receipt = ph.store.enqueue_note(tid, 'note-9', {'source': {'kind': 'agent', 'id': 'cutex.lead'},
        'eventType': 'agent_message', 'text': 'Message Type: MESSAGE\nSender: cutex.lead\nPayload:\nfyi',
        'metadata': {'deliveryMode': 'passive'}, 'view': VIEW}, 10)
    assert receipt['turnId'] is None
    # A subagent's tool call never takes the note.
    assert await hook({**data, 'agent_id': 'sub-1'}, 'call-1', {}) == {}
    result = await hook(data, 'call-2', {})
    context = result['hookSpecificOutput']['additionalContext']
    assert result['hookSpecificOutput']['hookEventName'] == 'PostToolUse'
    assert context.startswith('EXTERNAL_INPUT_JSON') and 'fyi' in context
    assert [i['id'] for i in ph.store.items(turn) if i['type'] == 'functionCallOutput'] == ['note-9']
    assert await hook(data, 'call-3', {}) == {}, 'at most once'


NOTE_TEXT = 'Message Type: MESSAGE\nSender: cutex.lead\nPayload:\nlog is in @/tmp/run.log'
WAKE_TEXT = 'Read and handle the unread notes other agents left for you.'


async def test_owner_wake_reads_pending_notes_in_its_first_input(ph):
    """hachimi 2026-10-19: the owner's 唤醒 (cutex/agentBus/wake) started a turn
    whose first request lacked the notes; they came only after the first tool call
    (PostToolUse), and a turn without tools never saw them."""
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    client = ph.sdk.clients[0]
    await ph.call('claude/input/submit', message(tid, 'note-a', NOTE_TEXT))
    await ph.call('claude/input/submit', message(tid, 'note-b', 'second note'))
    await ph.drain()
    assert client.query_count == 0, 'notes start no turn'
    turn = (await ph.call('turn/start', {'threadId': tid, 'clientUserMessageId': 'agent-bus-wake-1',
                                         'input': [{'type': 'text', 'text': WAKE_TEXT}]}))['turn']['id']
    await ph.drain()
    [sent] = client.messages
    content = sent['message']['content']
    assert [b['type'] for b in content] == ['text', 'text', 'text']
    assert content[0]['text'].startswith('EXTERNAL_INPUT_JSON') and 'log is in @/tmp/run.log' in content[0]['text']
    assert 'second note' in content[1]['text']
    # The owner's text is the last block, the only one Claude Code reads as typed
    # input (@path mentions): the notes' text is passed on as written.
    assert content[2] == {'type': 'text', 'text': WAKE_TEXT}
    assert 'client_composed' not in sent
    assert ph.store.pending_notes(tid) == 0
    items = ph.store.items(turn)
    assert [i['type'] for i in items][:3] == ['userMessage', 'functionCallOutput', 'functionCallOutput']
    assert [i['id'] for i in items if i['type'] == 'functionCallOutput'] == ['note-a', 'note-b']
    started = [m['params']['item']['id'] for m in ph.out if m.get('method') == 'item/completed'
               and m['params']['item']['type'] == 'functionCallOutput']
    assert started == ['note-a', 'note-b'], 'cutex reads the delivery from these items'
    for mid in ('note-a', 'note-b'):
        assert (await ph.call('claude/input/status', {'threadId': tid, 'messageId': mid}))['state'] == 'injected'


async def test_owner_turn_notes_are_not_offered_again_by_post_tool_use(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    client = ph.sdk.clients[0]
    await ph.call('claude/input/submit', message(tid, 'note-c', 'fyi'))
    ph.store.enqueue(tid, 'owner-1', {'source': {'kind': 'user', 'id': 'local-controller'},
                                      'eventType': 'user.message', 'text': 'see @hello.txt', 'metadata': {}}, 10)
    record = ph.store.claim(tid)
    turn = record['turn_id']
    assert record['prompt'] == 'see @hello.txt', 'the owner text stays as written'
    assert ph.store.turn_note_frames(turn)[0].startswith('EXTERNAL_INPUT_JSON')
    bridge = ph.engine.driver.owners[tid].bridge
    bridge.begin_turn(turn)
    hook = client.options.hooks['PostToolUse'][0].hooks[0]
    data = {'session_id': bridge.sid, 'hook_event_name': 'PostToolUse', 'tool_response': 'ok'}
    assert await hook(data, 'call-1', {}) == {}, 'read at the turn start: never offered again'
    assert [i['id'] for i in ph.store.items(turn) if i['type'] == 'functionCallOutput'] == ['note-c']


async def test_owner_turn_with_images_and_compact_keep_their_shape(ph, tmp_path):
    from test_desktop_parity import PNG
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    client = ph.sdk.clients[0]
    await ph.call('claude/input/submit', message(tid, 'note-d', 'fyi'))
    await ph.call('turn/start', {'threadId': tid, 'input': [
        {'type': 'text', 'text': 'look'}, {'type': 'image', 'url': 'data:image/png;base64,' + PNG}]})
    await ph.drain()
    content = client.messages[-1]['message']['content']
    assert [b['type'] for b in content] == ['text', 'text', 'image']
    assert 'fyi' in content[0]['text'] and content[1] == {'type': 'text', 'text': 'look'}
    # No note pending: an owner turn's message is unchanged (a plain string).
    await ph.turn(tid, 'plain'); await ph.drain()
    assert client.messages[-1]['message']['content'] == 'plain'
    # /compact reads no note: it waits for the next turn.
    await ph.call('claude/input/submit', message(tid, 'note-e', 'later'))
    ph.store.enqueue(tid, 'compact-1', {'source': {'kind': 'user', 'id': 'local-controller'},
                                     'eventType': 'thread.compact', 'text': '/compact', 'metadata': {}}, 10)
    claimed = ph.store.claim(tid, 'compact-1')
    assert ph.store.turn_note_frames(claimed['turn_id']) == [] and ph.store.pending_notes(tid) == 1
