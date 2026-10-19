"""Offline r8 runtime invariants; real fixture subprocesses, no model calls."""
import asyncio
from dataclasses import replace
from types import SimpleNamespace
import uuid

import pytest

from cute_claude.provider import ProviderError
from cute_claude.sdk_runtime import Control, End, NativeSessionLease
from test_persistent import ph, until
from test_questions_plan import begin


async def test_reconnect_keeps_logical_identity_and_accepts_only_fresh_input(ph):
    tid = await ph.thread()
    before = await ph.engine.start_runtime(tid)
    owner = ph.engine.driver.owners[tid]
    failed = await ph.turn(tid, 'CRASH')
    await ph.drain()
    after = owner.snapshot()
    assert after['connectionId'] == before['connectionId']
    assert after['sessionId'] == before['sessionId']
    assert after['nativeConnectionGeneration'] == 2 and after['automaticReconnect']
    assert ph.store.turn(failed)['status'] == 'failed'
    assert ph.sdk.clients[0].query_count == 1 and ph.sdk.clients[1].query_count == 0
    assert ph.sdk.clients[0]._transport._process.returncode is not None
    with pytest.raises(ProviderError, match='Another adapter'):
        NativeSessionLease(ph.engine.driver.config_dir, before['sessionId'])
    fresh = await ph.turn(tid, 'fresh instruction')
    await ph.drain()
    assert ph.store.turn(fresh)['status'] == 'completed'
    assert ph.sdk.clients[1].query_count == 1 and owner.reconnect_attempts == 0


async def test_reconnect_is_bounded_without_a_successful_turn(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    owner = ph.engine.driver.owners[tid]
    ph.sdk.clients[0]._transport._process.kill()
    await until(lambda: owner.native_connection_generation == 2)
    ph.sdk.clients[1]._transport._process.kill()
    await until(lambda: owner.task.done())
    assert len(ph.sdk.clients) == 2
    assert owner.snapshot()['state'] == 'closed'
    assert sum(client.query_count for client in ph.sdk.clients) == 0


async def test_reconnect_refuses_unverified_old_child(ph, monkeypatch):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    owner = ph.engine.driver.owners[tid]
    client = ph.sdk.clients[0]
    disconnect = client.disconnect
    # A real SDK could lose its process handle. Losing observation must not
    # permit a second writer even when disconnect itself returns successfully.
    owner.process = SimpleNamespace(pid=client._transport._process.pid, returncode=None)
    await owner.mailbox.put(End())
    await until(lambda: owner.task.done())
    assert len(ph.sdk.clients) == 1
    assert owner.snapshot()['state'] == 'close_unverified' and owner.lease is not None
    with pytest.raises(ProviderError, match='unverified'):
        await ph.engine.start_runtime(tid)
    # The fake child really exited; restore its diagnostic handle so fixture
    # cleanup can release the deliberately retained cooperative lock.
    assert client._transport._process.returncode is not None
    owner.lease.close()
    owner.lease = None


async def test_failed_reconnect_rejects_unexecuted_controls_without_success_receipts(ph, monkeypatch):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    owner, client = ph.engine.driver.owners[tid], ph.sdk.clients[0]

    async def fail_replacement(*args, **kwargs):
        raise ProviderError('Fixture replacement connection failed')

    monkeypatch.setattr(ph.engine.driver, 'make_client', fail_replacement)
    kinds = ['rewind_files', 'mcp_reconnect', 'mcp_toggle', 'task_stop',
             'mcp_status', 'task_list', 'context_usage', 'plan_usage',
             'permissions', 'model', 'model_effort']
    futures = {kind: asyncio.get_running_loop().create_future() for kind in kinds + ['close', 'interrupt']}
    # Queue the stream failure before every control without yielding. Recovery
    # fails after reaping the old process, so none of these can reach the SDK.
    owner.mailbox.put_nowait(End())
    for kind, future in futures.items():
        owner.mailbox.put_nowait(Control(kind, future, arguments={'user_message_id': 'never-rewound'}))
    await asyncio.wait_for(asyncio.shield(owner.task), 5)
    replies = dict(zip(futures, await asyncio.gather(*futures.values(), return_exceptions=True)))
    for kind in kinds:
        assert isinstance(replies[kind], ProviderError)
        assert f'control {kind!r} was executed' in str(replies[kind])
    assert replies['close'] is None and replies['interrupt'] is None
    assert client.controls == [] and client.query_count == 0
    assert client._transport._process.returncode is not None
    assert owner.lease is None and len(ph.sdk.clients) == 1


async def test_child_callbacks_need_registration_and_share_host_bridge(ph):
    tid, turn, bridge = await begin(ph)
    data = {'session_id': bridge.sid, 'agent_id': 'child-1',
            'tool_name': 'mcp__cutex__read_file', 'tool_input': {'arguments': {'path': 'hello.txt'}}}
    denied = await bridge.pre_tool(data, 'child-read', {})
    assert denied['hookSpecificOutput']['permissionDecision'] == 'deny'
    await bridge.subagent_start({'session_id': bridge.sid, 'agent_id': 'child-1'}, None, {})
    permit = await bridge.pre_tool(data, 'child-read', {})
    args = permit['hookSpecificOutput']['updatedInput']
    result = await bridge.execute('read_file', args)
    assert not result['is_error'] and 'persistent fixture' in result['content'][0]['text']
    row = ph.store.db.execute('SELECT turn_id FROM tool_calls WHERE call_id=?', ('child-read',)).fetchone()
    assert row[0] == turn
    items = [item for item in ph.store.items(turn) if item['type'] == 'subAgentActivity']
    assert items[0]['kind'] == 'started'
    assert set(items[0]) == {'type', 'id', 'kind', 'agentThreadId', 'agentPath'}
    await bridge.subagent_stop({'session_id': bridge.sid, 'agent_id': 'child-1'}, None, {})
    assert (await bridge.pre_tool(data, 'late', {}))['hookSpecificOutput']['permissionDecision'] == 'deny'
    assert ph.store.items(turn)[-1]['kind'] == 'completed'


async def test_child_native_tools_keep_permissions_and_post_identity(ph):
    tid, turn, bridge = await begin(ph)
    await bridge.subagent_start({'session_id': bridge.sid, 'agent_id': 'child'}, None, {})
    data = {'session_id': bridge.sid, 'agent_id': 'child', 'tool_name': 'Read',
            'tool_input': {'file_path': str(ph.settings.workspace / 'hello.txt')}}
    assert await bridge.pre_tool(data, 'read', {}) == {}
    assert bridge.permission_context_valid('read', 'child')
    assert not bridge.permission_context_valid('read', 'other')
    await bridge.native.post({'session_id': bridge.sid, 'tool_response': 'wrong context'}, 'read', {})
    assert 'read' in bridge.native.calls
    await bridge.native.post({**data, 'tool_response': {'file': {'content': 'child result'}}}, 'read', {})
    assert ph.store.items(turn)[-1]['aggregatedOutput'] == 'child result'
    for name in ['Write', 'Bash', 'SendMessage', 'ListAgents', 'SendFile']:
        denied = await bridge.pre_tool({**data, 'tool_name': name}, name, {})
        assert denied['hookSpecificOutput']['permissionDecision'] == 'deny'


async def test_agent_tool_inherits_policy_and_cannot_create_unbound_turns(ph):
    tid, turn, bridge = await begin(ph)
    ph.store.db.execute('INSERT OR REPLACE INTO thread_tool_approvals VALUES(?,?)', (tid, 'default'))
    data = {'session_id': bridge.sid, 'tool_name': 'Agent', 'tool_input': {'prompt': 'analyze', 'run_in_background': False}}
    assert await bridge.pre_tool(data, 'agent', {}) == {}
    # Leaving run_in_background out is a background subagent in newer Claude Code.
    for args in [{'mode': 'bypassPermissions', 'run_in_background': False}, {'run_in_background': True}, {'prompt': 'analyze'}]:
        denied = await bridge.pre_tool({**data, 'tool_input': args}, str(uuid.uuid4()), {})
        assert denied['hookSpecificOutput']['permissionDecision'] == 'deny'
    assert ph.sdk.clients[0].options.env['CLAUDE_CODE_HARBOR_KITE'] == '0'


async def test_child_stream_never_becomes_parent_output_or_result(ph):
    tid = await ph.thread()
    turn = await ph.turn(tid, 'HANG')
    await until(lambda: ph.sdk.clients and ph.sdk.clients[0].query_count == 1)
    client, owner = ph.sdk.clients[0], ph.engine.driver.owners[tid]
    await client.events.put({'type': 'assistant', 'parent_tool_use_id': 'agent-call',
                             'content': [{'type': 'text', 'text': 'CHILD PRIVATE REPLY'}]})
    await client.events.put({'type': 'result', 'parent_tool_use_id': 'agent-call', 'session_id': owner.session_id,
                             'subtype': 'success'})
    await client.events.put({'type': 'assistant', 'content': [{'type': 'text', 'text': 'parent reply'}]})
    await client.events.put({'type': 'result', 'session_id': owner.session_id, 'subtype': 'success',
                             'is_error': False, 'terminal_reason': 'completed', 'usage': {}})
    await ph.drain()
    messages = [item['text'] for item in ph.store.items(turn) if item['type'] == 'agentMessage']
    assert messages == ['parent reply'] and ph.store.turn(turn)['status'] == 'completed'


async def test_task_lifecycle_uses_origin_turn_and_handles_updated_killed(ph):
    tid, turn, bridge = await begin(ph)
    owner = ph.engine.driver.owners[tid]
    await owner.tasks.observe({'type': 'system', 'subtype': 'task_started', 'task_id': 'task-a',
                               'task_type': 'local_agent', 'session_id': bridge.sid}, turn)
    await owner.tasks.observe({'type': 'TaskProgressMessage', 'task_id': 'task-a',
                               'usage': {'total_tokens': 12, 'secret': 'omit'}}, None)
    await owner.tasks.observe({'type': 'TaskUpdatedMessage', 'task_id': 'task-a', 'patch': {'status': 'killed',
                               'secret': 'omit'}}, None)
    await owner.tasks.observe({'type': 'TaskNotificationMessage', 'task_id': 'task-a', 'status': 'stopped'}, None)
    items = [item for item in ph.store.items(turn) if item['type'] == 'subAgentActivity']
    assert [item['kind'] for item in items] == ['started', 'interacted', 'interrupted']
    snapshot = await ph.engine.runtime_control(tid, 'task_list')
    assert snapshot['tasks'][0]['status'] == 'killed' and not snapshot['completeInventory']
    assert 'secret' not in str(snapshot)
    with pytest.raises(ProviderError, match='known running'):
        await ph.engine.runtime_control(tid, 'task_stop', task_id='task-a')


async def test_resumed_child_retains_identity_with_new_origin_turn(ph):
    tid, first, bridge = await begin(ph)
    data = {'session_id': bridge.sid, 'agent_id': 'resumable-child'}
    await bridge.subagent_start(data, None, {})
    await bridge.subagent_stop(data, None, {})
    await bridge.end_turn()
    ph.store.finish(tid, first, 'completed')
    ph.store.enqueue(tid, str(uuid.uuid4()), {'text': 'continue child', 'eventType': 'message',
                     'source': {'kind': 'user', 'id': 'fixture'}}, 10)
    second = ph.store.claim(tid)['turn_id']
    bridge.begin_turn(second)
    await bridge.subagent_start(data, None, {})
    await bridge.subagent_stop(data, None, {})
    first_items = [item for item in ph.store.items(first) if item['type'] == 'subAgentActivity']
    second_items = [item for item in ph.store.items(second) if item['type'] == 'subAgentActivity']
    assert [item['kind'] for item in second_items] == ['interacted', 'completed']
    assert first_items[0]['agentThreadId'] == second_items[0]['agentThreadId']
    owner = ph.engine.driver.owners[tid]
    await owner.tasks.observe({'type': 'TaskNotificationMessage', 'task_id': 'resumable-child',
                               'status': 'failed'}, None)
    assert owner.tasks.snapshot()['tasks'][0]['status'] == 'failed'
    assert ph.store.items(second)[-1]['kind'] == 'interrupted'


async def test_controls_use_sdk_owner_task_and_rewind_is_idle_only(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    client = ph.sdk.clients[0]
    assert (await ph.engine.runtime_control(tid, 'mcp_status'))['mcpServers'][0]['status'] == 'connected'
    await ph.engine.runtime_control(tid, 'mcp_reconnect', server_name='cutex')
    await ph.engine.runtime_control(tid, 'mcp_toggle', server_name='cutex', enabled=False)
    await ph.engine.runtime_control(tid, 'rewind_files', user_message_id='checkpoint')
    assert client.controls[-3:] == [('mcp_reconnect', 'cutex'), ('mcp_toggle', 'cutex', False), ('rewind_files', 'checkpoint')]
    turn = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    with pytest.raises(ProviderError, match='Finish or interrupt'):
        await ph.engine.runtime_control(tid, 'rewind_files', user_message_id='checkpoint')
    await ph.engine.interrupt(tid, turn)


async def test_background_bash_observation_and_stop(ph):
    tid, turn, bridge = await begin(ph)
    bridge.engine.settings = replace(ph.settings, allow_exec=True)
    args = {'command': 'fixture-background', 'run_in_background': True}
    await bridge.pre_tool({'session_id': bridge.sid, 'tool_name': 'Bash', 'tool_input': args}, 'bash-call', {})
    await bridge.native.post({'session_id': bridge.sid, 'tool_response': {'stdout': '', 'backgroundTaskId': 'bash-task'}}, 'bash-call', {})
    snapshot = await ph.engine.runtime_control(tid, 'task_list')
    task = snapshot['tasks'][0]
    assert task['command'] == args['command'] and task['itemId'] == 'bash-call'
    assert task['taskType'] == 'local_bash' and task['turnId'] == turn
    await ph.engine.runtime_control(tid, 'task_stop', task_id='bash-task')
    assert ph.sdk.clients[0].controls[-1] == ('task_stop', 'bash-task')
    assert not any(item['type'] == 'subAgentActivity' for item in ph.store.items(turn))


async def test_native_message_uuids_are_checkpointed_and_success_receipt_is_emitted(ph):
    tid = await ph.thread()
    turn = await ph.turn(tid, 'HANG')
    await until(lambda: ph.sdk.clients and ph.sdk.clients[0].query_count == 1)
    client = ph.sdk.clients[0]
    uid, aid = str(uuid.uuid4()), str(uuid.uuid4())
    sid = ph.engine.session(tid)['session_id']
    await client.events.put({'type': 'user', 'session_id': sid, 'uuid': uid, 'content': 'HANG'})
    await client.events.put({'type': 'assistant', 'uuid': aid, 'content': [{'type': 'text', 'text': 'ok'}]})
    await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': False,
                             'terminal_reason': 'completed', 'usage': {}})
    await ph.drain()
    rows = ph.store.db.execute('SELECT native_uuid,role FROM sdk_turn_messages WHERE turn_id=? ORDER BY rowid', (turn,)).fetchall()
    assert [tuple(row) for row in rows] == [(uid, 'user'), (aid, 'assistant')]
    assert any(event['method'] == 'claude/input/contextPersisted' and event['params']['turnId'] == turn
               for event in ph.store.events(tid, 0, 200))
