"""Foreground subagent transcripts (features.subagentTranscripts); fake SDK, no model calls."""
from cute_claude import subagent_transcripts
from cute_claude.subagent_transcripts import SubagentTranscripts, parse_reference
from test_persistent import ph, until  # noqa: F401  (fixture)
from test_questions_plan import begin

AGENT_INPUT = {'prompt': 'Read the docs and summarise them', 'description': 'Review docs', 'subagent_type': 'Explore', 'run_in_background': False}


async def spawn(bridge, call_id='agent-call', arguments=AGENT_INPUT):
    assert await bridge.pre_tool({'session_id': bridge.sid, 'tool_name': 'Agent', 'tool_input': arguments}, call_id, {}) == {}


def child_messages(owner, call_id='agent-call', tool_id='child-read'):
    sid = owner.session_id
    return [
        {'type': 'assistant', 'parent_tool_use_id': call_id, 'session_id': sid, 'uuid': f'{tool_id}-a',
         'content': [{'type': 'thinking', 'thinking': 'Plan: read the file.'},
                     {'type': 'text', 'text': 'Reading the docs now.'},
                     {'type': 'tool_use', 'id': tool_id, 'name': 'Read', 'input': {'file_path': '/w/docs.md'}}]},
        {'type': 'user', 'parent_tool_use_id': call_id, 'session_id': sid, 'uuid': f'{tool_id}-u',
         'content': [{'type': 'tool_result', 'tool_use_id': tool_id, 'content': '# Docs\nhello', 'is_error': False}]},
        {'type': 'assistant', 'parent_tool_use_id': call_id, 'session_id': sid, 'uuid': f'{tool_id}-b',
         'content': [{'type': 'text', 'text': 'The docs say hello.'}]},
    ]


def views(ph, ref):
    return [m for m in ph.out if isinstance(m.get('params'), dict) and m['params'].get('threadId') == ref]


async def test_transcript_is_stored_streamed_and_read_with_parent_lineage(ph):
    tid, turn, bridge = await begin(ph)
    owner = ph.engine.driver.owners[tid]
    await spawn(bridge)
    await owner.tasks.observe({'type': 'system', 'subtype': 'task_started', 'task_id': 'task-a', 'task_type': 'local_agent',
                               'tool_use_id': 'agent-call', 'description': 'Review docs', 'session_id': bridge.sid}, turn)
    ref = f'claude:{bridge.sid}:task:task-a'
    activity = [i for i in ph.store.items(turn) if i['type'] == 'subAgentActivity']
    assert activity and activity[0]['agentThreadId'] == ref
    for message in child_messages(owner):
        await owner._event(message)
    # Never the parent's reply.
    assert not [i for i in ph.store.items(turn) if i['type'] == 'agentMessage']
    read = await ph.call('thread/read', {'threadId': ref, 'includeTurns': True})
    thread = read['thread']
    assert thread['id'] == ref and thread['parentThreadId'] == tid
    assert thread['source']['subAgent']['thread_spawn']['parent_thread_id'] == tid
    assert thread['agentNickname'] == 'Review docs' and thread['agentRole'] == 'Explore'
    assert thread['status'] == {'type': 'active', 'activeFlags': []}
    items = thread['turns'][0]['items']
    assert [i['type'] for i in items] == ['userMessage', 'reasoning', 'agentMessage', 'commandExecution', 'agentMessage']
    assert items[0]['content'][0]['text'] == AGENT_INPUT['prompt']
    assert items[3]['status'] == 'completed' and items[3]['aggregatedOutput'] == '# Docs\nhello'
    # Live frames reach the parent's subscribers under the subagent reference ...
    methods = [m['method'] for m in views(ph, ref)]
    assert methods[0] == 'turn/started' and 'item/started' in methods and 'item/completed' in methods
    # ... but are not logged as the parent thread's events.
    assert all(e['params'].get('threadId') != ref for e in ph.store.events(tid, 0, 1000))
    # The spawning call finishes: the run closes and the tool card points at the reference.
    await bridge.native.post({'session_id': bridge.sid, 'hook_event_name': 'PostToolUse', 'tool_response': 'summary'}, 'agent-call', {})
    card = next(i for i in ph.store.items(turn) if i['id'] == 'agent-call')
    assert card['type'] == 'dynamicToolCall' and card['agentThreadId'] == ref and card['status'] == 'completed'
    read = await ph.call('thread/read', {'threadId': ref, 'includeTurns': False})
    assert read['thread']['status'] == {'type': 'idle'} and read['thread']['turns'] == []
    assert views(ph, ref)[-1]['method'] == 'turn/completed'
    assert views(ph, ref)[-1]['params']['turn']['status'] == 'completed'
    # The spawning call id resolves to the same transcript.
    by_call = await ph.call('thread/read', {'threadId': f'claude:{bridge.sid}:task:agent-call', 'includeTurns': True})
    assert len(by_call['thread']['turns'][0]['items']) == 5


async def test_unknown_references_and_foreign_sessions_are_not_found(ph):
    tid, turn, bridge = await begin(ph)
    await spawn(bridge)
    for ref in ['claude:other-session:task:agent-call', f'claude:{bridge.sid}:task:nope', 'claude:x', tid + 'x']:
        error = await ph.call('thread/read', {'threadId': ref}, error=True)
        assert error['code'] == -32004, ref
    assert parse_reference('claude:s:task:k') == ('s', 'k') and parse_reference('claude:s') is None


async def test_unlinked_run_opens_by_call_id_and_turn_end_interrupts(ph):
    tid, turn, bridge = await begin(ph)
    owner = ph.engine.driver.owners[tid]
    await spawn(bridge, 'call-1')
    await spawn(bridge, 'call-2', {**AGENT_INPUT, 'description': 'Second'})
    # Two open runs: SubagentStart alone cannot tell which child is which.
    for child in ('child-1', 'child-2'):
        await bridge.subagent_start({'session_id': bridge.sid, 'agent_id': child}, None, {})
    assert ph.engine.subagents.resolve(f'claude:{bridge.sid}:task:child-2') is None
    # A pending child reads as a running, empty subagent of this thread.
    pending = await ph.call('thread/read', {'threadId': f'claude:{bridge.sid}:task:child-2'})
    assert pending['thread']['parentThreadId'] == tid and pending['thread']['status']['type'] == 'active'
    # Its tool call seen in the stream (under call-2) and in a hook (as child-2) links them.
    for message in child_messages(owner, 'call-2', 'c2-read')[:1]:
        await owner._event(message)
    data = {'session_id': bridge.sid, 'agent_id': 'child-2', 'tool_name': 'Read', 'tool_input': {'file_path': '/w/docs.md'}}
    assert await bridge.pre_tool(data, 'c2-read', {}) == {}
    row = ph.engine.subagents.resolve(f'claude:{bridge.sid}:task:child-2')
    assert row is not None and row['call_id'] == 'call-2'
    # The turn ends with both still running: interrupted, open tool items closed.
    await bridge.end_turn()
    read = await ph.call('thread/read', {'threadId': f'claude:{bridge.sid}:task:child-2', 'includeTurns': True})
    turn_wire = read['thread']['turns'][0]
    assert turn_wire['status'] == 'interrupted'
    assert all(i.get('status') != 'inProgress' for i in turn_wire['items'])
    assert ph.engine.subagents.resolve(f'claude:{bridge.sid}:task:call-1')['status'] == 'interrupted'


async def test_single_open_run_links_from_subagent_start_and_survives_restart(ph):
    tid, turn, bridge = await begin(ph)
    owner = ph.engine.driver.owners[tid]
    await spawn(bridge)
    await bridge.subagent_start({'session_id': bridge.sid, 'agent_id': 'only-child'}, None, {})
    ref = f'claude:{bridge.sid}:task:only-child'
    for message in child_messages(owner):
        await owner._event(message)
    assert len((await ph.call('thread/read', {'threadId': ref, 'includeTurns': True}))['thread']['turns'][0]['items']) == 5
    # A new adapter process: the transcript is still there, the lost run is interrupted.
    ph.engine.subagents = SubagentTranscripts(ph.engine)
    read = await ph.call('thread/read', {'threadId': ref, 'includeTurns': True})
    assert read['thread']['status'] == {'type': 'idle'}
    assert read['thread']['turns'][0]['status'] == 'interrupted'
    assert len(read['thread']['turns'][0]['items']) == 5


async def test_transcripts_are_bounded(ph, monkeypatch):
    monkeypatch.setattr(subagent_transcripts, 'MAX_ITEMS', 3)
    monkeypatch.setattr(subagent_transcripts, 'MAX_RUNS', 2)
    tid, turn, bridge = await begin(ph)
    owner = ph.engine.driver.owners[tid]
    await spawn(bridge, 'a1')
    for message in child_messages(owner, 'a1'):
        await owner._event(message)
    row = ph.engine.subagents.resolve(f'claude:{bridge.sid}:task:a1')
    assert row['truncated'] == 1 and len(ph.engine.subagents.items('a1')) == 3
    assert (await ph.call('thread/read', {'threadId': f'claude:{bridge.sid}:task:a1'}))['thread']['transcriptTruncated']
    await spawn(bridge, 'a2')
    await spawn(bridge, 'a3')
    assert ph.engine.subagents.resolve(f'claude:{bridge.sid}:task:a1') is None
    assert ph.store.db.execute("SELECT count(*) FROM subagent_items WHERE call_id='a1'").fetchone()[0] == 0


async def test_subagent_text_forwarding_is_requested(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert ph.sdk.clients[0].options.forward_subagent_text is True
    features = (await ph.call('claude/capabilities'))['features']
    assert features['subagentTranscripts'] is True
