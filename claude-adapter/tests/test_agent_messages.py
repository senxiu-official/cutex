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


async def test_owner_turn_leaves_notes_pending_and_bad_views_are_refused(h):
    tid = await h.thread()
    await h.call('claude/input/submit', message(tid, 'note-2', 'fyi'))
    turn = await h.turn(tid, 'hello')
    await h.drain()
    assert turn in turns_started(h)
    assert h.store.pending_notes(tid) == 1, 'owner text stays as written; the note waits'
    taken = h.store.take_notes(tid, turn)
    assert [item['id'] for item, _ in taken] == ['note-2'] and h.store.pending_notes(tid) == 0
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
