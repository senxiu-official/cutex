"""Codex v2 shapes the cutex desktop renders, produced from Claude SDK traffic.

Offline only: the fake SDK + fixture process and the scripted driver stand in
for Claude Code; no model, login or network is used.
"""
import asyncio
import base64
import json
from dataclasses import replace
from types import SimpleNamespace

import pytest

from cute_claude.store import identifier
from test_persistent import ph
from test_questions_plan import begin
from test_subscription import sh, emit_reply

PNG = base64.b64encode(b'\x89PNG\r\n\x1a\n' + b'0' * 64).decode()


def sent(h, method):
    return [m for m in h.out if m.get('method') == method]


def items_of(h, turn, kind):
    return [i for i in h.store.items(turn) if i['type'] == kind]


async def wait_request(h, method):
    async with asyncio.timeout(2):
        while not sent(h, method):
            await asyncio.sleep(.001)
    return sent(h, method)[-1]


# ---- native tools -> commandExecution / fileChange / plan -------------------

async def test_bash_projects_to_command_execution_with_output_and_exit_code(ph):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Bash', 'tool_input': {'command': 'echo hi'}}, 'ok-call', {})
    started = sent(ph, 'item/started')[-1]['params']['item']
    assert started['type'] == 'commandExecution' and started['status'] == 'inProgress'
    assert started['command'] == 'echo hi' and started['cwd'] == str(ph.settings.workspace)
    assert started['source'] == 'agent' and started['exitCode'] is None
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse',
                         'tool_response': {'stdout': 'hi\n', 'stderr': '', 'interrupted': False}}, 'ok-call', {})
    done = items_of(ph, turn, 'commandExecution')[-1]
    assert (done['status'], done['exitCode'], done['aggregatedOutput']) == ('completed', 0, 'hi\n')
    assert isinstance(done['durationMs'], int)
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Bash', 'tool_input': {'command': 'false'}}, 'bad-call', {})
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUseFailure',
                         'error': 'Exit code 2\nno such file'}, 'bad-call', {})
    failed = next(i for i in items_of(ph, turn, 'commandExecution') if i['id'] == 'bad-call')
    assert (failed['status'], failed['exitCode']) == ('failed', 2)
    assert 'no such file' in failed['aggregatedOutput']


async def test_read_grep_glob_are_commands_with_actions(ph):
    tid, turn, b = await begin(ph)
    path = str(ph.settings.workspace / 'hello.txt')
    for call, name, args in [('r', 'Read', {'file_path': path}), ('g', 'Grep', {'pattern': 'TODO', 'path': 'src'}),
                             ('l', 'Glob', {'pattern': '**/*.py'})]:
        await b.pre_tool({'session_id': b.sid, 'tool_name': name, 'tool_input': args}, call, {})
    read, grep, glob = items_of(ph, turn, 'commandExecution')
    assert read['commandActions'] == [{'type': 'read', 'command': f'Read {path}', 'name': 'hello.txt', 'path': path}]
    assert grep['commandActions'][0]['type'] == 'search' and grep['commandActions'][0]['query'] == 'TODO'
    assert glob['commandActions'][0]['type'] == 'listFiles' and glob['command'] == 'Glob **/*.py'
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse',
                         'tool_response': {'type': 'text', 'file': {'filePath': path, 'content': 'file body'}}}, 'r', {})
    assert items_of(ph, turn, 'commandExecution')[0]['aggregatedOutput'] == 'file body'


async def test_edit_and_write_project_to_file_changes_and_turn_diff(ph):
    ph.engine.settings = replace(ph.settings, allow_write=True)
    tid, turn, b = await begin(ph)
    target = ph.settings.workspace / 'notes.txt'
    target.write_text('one\ntwo\nthree\n')
    args = {'file_path': str(target), 'old_string': 'two', 'new_string': 'TWO'}
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Edit', 'tool_input': args}, 'edit', {})
    proposed = sent(ph, 'item/started')[-1]['params']['item']
    assert proposed['type'] == 'fileChange' and proposed['status'] == 'inProgress'
    [change] = proposed['changes']
    assert change['path'] == str(target) and change['kind'] == {'type': 'update', 'move_path': None}
    assert '-two\n' in change['diff'] and '+TWO\n' in change['diff']
    target.write_text('one\nTWO\nthree\n')  # the official runtime applies the edit
    patch = [{'oldStart': 1, 'oldLines': 3, 'newStart': 1, 'newLines': 3, 'lines': [' one', '-two', '+TWO', ' three']}]
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse',
                         'tool_response': {'filePath': str(target), 'structuredPatch': patch}}, 'edit', {})
    done = items_of(ph, turn, 'fileChange')[-1]
    assert done['status'] == 'completed'
    assert done['changes'][0]['diff'] == '@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n'
    diff = sent(ph, 'turn/diff/updated')[-1]['params']
    assert diff['turnId'] == turn and 'diff --git a/notes.txt b/notes.txt' in diff['diff']
    assert '-two' in diff['diff'] and '+TWO' in diff['diff']
    new_file = ph.settings.workspace / 'new.txt'
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Write',
                      'tool_input': {'file_path': str(new_file), 'content': 'hello\n'}}, 'write', {})
    assert sent(ph, 'item/started')[-1]['params']['item']['changes'][0]['kind'] == {'type': 'add'}
    new_file.write_text('hello\n')
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse',
                         'tool_response': {'type': 'create', 'filePath': str(new_file), 'content': 'hello\n'}}, 'write', {})
    combined = sent(ph, 'turn/diff/updated')[-1]['params']['diff']
    assert 'diff --git a/notes.txt' in combined and '--- /dev/null' in combined and '+++ b/new.txt' in combined


async def test_todowrite_and_task_tools_update_the_plan_checklist(ph):
    tid, turn, b = await begin(ph)
    todos = [{'content': 'Read code', 'status': 'completed', 'activeForm': 'Reading'},
             {'content': 'Fix bug', 'status': 'in_progress', 'activeForm': 'Fixing'},
             {'content': 'Test', 'status': 'pending', 'activeForm': 'Testing'}]
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'TodoWrite', 'tool_input': {'todos': todos}}, 'todo', {})
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse',
                         'tool_response': {'oldTodos': [], 'newTodos': todos}}, 'todo', {})
    update = sent(ph, 'turn/plan/updated')[-1]['params']
    assert update == {'threadId': tid, 'turnId': turn, 'explanation': None, 'plan': [
        {'step': 'Read code', 'status': 'completed'}, {'step': 'Fix bug', 'status': 'inProgress'},
        {'step': 'Test', 'status': 'pending'}]}
    assert not [i for i in ph.store.items(turn) if i.get('tool') == 'TodoWrite']
    # Task* tools keep a task list across calls (and turns).
    for call, name, args, response in [
            ('c1', 'TaskCreate', {'subject': 'Write tests', 'description': 'x'}, {'task': {'id': '1', 'subject': 'Write tests'}}),
            ('c2', 'TaskCreate', {'subject': 'Ship', 'description': 'y'}, {'task': {'id': '2', 'subject': 'Ship'}}),
            ('u1', 'TaskUpdate', {'taskId': '1', 'status': 'in_progress'}, {'success': True, 'taskId': '1'})]:
        await b.pre_tool({'session_id': b.sid, 'tool_name': name, 'tool_input': args}, call, {})
        await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse', 'tool_response': response}, call, {})
    assert sent(ph, 'turn/plan/updated')[-1]['params']['plan'] == [
        {'step': 'Write tests', 'status': 'inProgress'}, {'step': 'Ship', 'status': 'pending'}]
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'TaskUpdate', 'tool_input': {'taskId': '2', 'status': 'deleted'}}, 'd', {})
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse', 'tool_response': {'success': True}}, 'd', {})
    assert sent(ph, 'turn/plan/updated')[-1]['params']['plan'] == [{'step': 'Write tests', 'status': 'inProgress'}]


# ---- approvals ----------------------------------------------------------------


def ask_mode(ph, tid, mode='default'):
    # These tests exercise the approval UI: leave full access (the default) for an asking mode.
    ph.server.store.db.execute('INSERT OR REPLACE INTO thread_tool_approvals(thread_id, mode) VALUES (?, ?)', (tid, mode))

@pytest.mark.parametrize('decision', ['accept', 'acceptForSession', 'decline', 'cancel'])
async def test_edit_permission_uses_file_change_approval(ph, decision):
    ph.engine.settings = replace(ph.settings, allow_write=True)
    tid, turn, b = await begin(ph)
    ask_mode(ph, tid)
    target = ph.settings.workspace / 'a.txt'
    target.write_text('x\n')
    args = {'file_path': str(target), 'content': 'y\n'}
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Write', 'tool_input': args}, 'w', {})
    can_use_tool = ph.sdk.clients[0].options.can_use_tool
    task = asyncio.create_task(can_use_tool('Write', args, SimpleNamespace(tool_use_id='w', title='Claude wants to write a.txt')))
    try:
        request = await wait_request(ph, 'item/fileChange/requestApproval')
        assert request['params']['itemId'] == 'w' and request['params']['reason'] == 'Claude wants to write a.txt'
        await ph.server.handle(ph.peer, {'id': request['id'], 'result': {'decision': decision}})
        result = await asyncio.wait_for(task, 2)
    finally:
        if not task.done():
            task.cancel(); await asyncio.gather(task, return_exceptions=True)
    if decision in {'accept', 'acceptForSession'}:
        assert result.behavior == 'allow'
        if decision == 'acceptForSession':
            [rule_update] = result.updated_permissions
            assert rule_update.destination == 'session' and rule_update.behavior == 'allow'
            assert {r.tool_name for r in rule_update.rules} == {'Edit', 'MultiEdit', 'NotebookEdit', 'Write'}
        else:
            assert result.updated_permissions is None
    else:
        assert result.behavior == 'deny' and result.interrupt is (decision == 'cancel')
        assert items_of(ph, turn, 'fileChange')[-1]['status'] == 'declined'
    assert sent(ph, 'serverRequest/resolved')


async def test_bash_session_rule_is_exact_command(ph):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    ask_mode(ph, tid)
    args = {'command': 'npm test'}
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Bash', 'tool_input': args}, 'b', {})
    task = asyncio.create_task(ph.sdk.clients[0].options.can_use_tool('Bash', args, SimpleNamespace(tool_use_id='b')))
    request = await wait_request(ph, 'item/commandExecution/requestApproval')
    assert request['params']['command'] == 'npm test' and request['params']['kind'] == 'command'
    await ph.server.handle(ph.peer, {'id': request['id'], 'result': {'decision': 'acceptForSession'}})
    result = await asyncio.wait_for(task, 2)
    [rule] = result.updated_permissions[0].rules
    assert (rule.tool_name, rule.rule_content) == ('Bash', 'npm test')


async def test_server_request_goes_to_every_interactive_client_first_answer_wins(ph):
    tid, turn, b = await begin(ph)
    second = ph.server.connect(ph.send)
    second.initialized = second.ready = second.interactive = True
    second.subscriptions.add(tid)
    observer = ph.server.connect(ph.send)
    observer.initialized = observer.ready = True
    observer.subscriptions.add(tid)  # subscribed but not interactive: no prompt
    task = asyncio.create_task(ph.server.approvals.request_command(tid, turn, 'item', 'make', None))
    request = await wait_request(ph, 'item/commandExecution/requestApproval')
    assert len(sent(ph, 'item/commandExecution/requestApproval')) == 2
    ph.server.disconnect(ph.peer)  # one client leaving does not decide
    await asyncio.sleep(.01)
    assert not task.done()
    await ph.server.handle(observer, {'id': request['id'], 'result': {'decision': 'accept'}})
    await asyncio.sleep(.01)
    assert not task.done()
    await ph.server.handle(second, {'id': request['id'], 'result': {'decision': 'decline'}})
    assert await asyncio.wait_for(task, 2) == 'decline'
    assert not ph.server.approvals.pending


async def test_no_interactive_client_means_no_decision(ph):
    tid, turn, b = await begin(ph)
    ph.peer.interactive = False
    assert await ph.server.approvals.request_file_change(tid, turn, 'x') is None


# ---- turn streaming: reasoning, usage, compaction -----------------------------

async def test_thinking_becomes_reasoning_summary_and_usage_is_last_call(sh):
    tid = await sh.thread()
    async def respond(**kw):
        emit, sid = kw['on_message'], kw['session_id']
        await emit({'type': 'stream_event', 'session_id': sid, 'event': {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'thinking'}}})
        await emit({'type': 'stream_event', 'session_id': sid, 'event': {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'thinking_delta', 'thinking': 'Let me think'}}})
        await emit({'type': 'assistant', 'session_id': sid, 'content': [{'type': 'thinking', 'thinking': 'Let me think', 'signature': 's'}],
                    'usage': {'input_tokens': 10, 'cache_read_input_tokens': 40000, 'cache_creation_input_tokens': 500, 'output_tokens': 20}})
        await emit({'type': 'assistant', 'session_id': sid, 'content': [{'type': 'text', 'text': 'Done.'}],
                    'usage': {'input_tokens': 12, 'cache_read_input_tokens': 41000, 'cache_creation_input_tokens': 300, 'output_tokens': 30}})
        await emit({'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': False, 'terminal_reason': 'completed',
                    'num_turns': 2, 'usage': {'input_tokens': 22, 'cache_read_input_tokens': 81000, 'cache_creation_input_tokens': 800, 'output_tokens': 50},
                    'model_usage': {'claude-sonnet': {'contextWindow': 200000}, 'claude-haiku': {'contextWindow': 200000}}})
    sh.provider.behavior = respond
    turn = await sh.turn(tid, 'think')
    await sh.drain()
    [reasoning] = items_of(sh, turn, 'reasoning')
    assert reasoning['summary'] == ['Let me think'] and reasoning['content'] == []
    delta = sent(sh, 'item/reasoning/summaryTextDelta')[-1]['params']
    assert delta['itemId'] == reasoning['id'] and delta['summaryIndex'] == 0
    usage = sent(sh, 'thread/tokenUsage/updated')[-1]['params']['tokenUsage']
    assert usage['modelContextWindow'] == 200000
    assert usage['last'] == {'totalTokens': 41342, 'inputTokens': 41312, 'cachedInputTokens': 41000,
                             'cacheWriteInputTokens': 300, 'outputTokens': 30, 'reasoningOutputTokens': 0}
    assert usage['total']['cacheWriteInputTokens'] == 800 and usage['total']['outputTokens'] == 50


async def test_compact_runs_slash_compact_without_user_bubble(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    client = ph.sdk.clients[0]
    sid = ph.engine.session(tid)['session_id']
    original = client.query
    async def compact(prompt):
        message = [m async for m in prompt][0]
        client.messages.append(message)
        client.query_count += 1
        await client.events.put({'type': 'system', 'subtype': 'compact_boundary', 'session_id': sid,
                                 'compact_metadata': {'trigger': 'manual', 'pre_tokens': 1000}})
        await client.events.put({'type': 'result', 'session_id': sid, 'subtype': 'success', 'is_error': False,
                                 'num_turns': 0, 'usage': {}})
    client.query = compact
    assert await ph.call('thread/compact/start', {'threadId': tid}) == {}
    await ph.drain()
    [turn] = [r[0] for r in ph.store.db.execute('SELECT id FROM turns WHERE thread_id=?', (tid,))]
    assert [i['type'] for i in ph.store.items(turn)] == ['contextCompaction']
    assert ph.store.turn(turn)['status'] == 'completed'
    assert client.messages[-1]['message']['content'] == '/compact' and 'client_composed' not in client.messages[-1]
    client.query = original
    busy = await ph.call('thread/compact/start', {'threadId': 'missing'}, error=True)
    assert busy['code'] in (-32004, -32009)


# ---- input: images, clientId, verbatim prompts --------------------------------

async def test_images_client_id_and_verbatim_prompts(ph, tmp_path):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    client = ph.sdk.clients[0]
    picture = tmp_path / 'shot.png'
    picture.write_bytes(base64.b64decode(PNG))
    data_url = 'data:image/png;base64,' + PNG
    turn = (await ph.call('turn/start', {'threadId': tid, 'clientUserMessageId': 'client-1', 'input': [
        {'type': 'text', 'text': 'look', 'text_elements': []}, {'type': 'image', 'url': data_url},
        {'type': 'localImage', 'path': str(picture)}]}))['turn']['id']
    await ph.drain()
    content = client.messages[-1]['message']['content']
    assert content[0] == {'type': 'text', 'text': 'look'}
    assert [b['source']['type'] for b in content[1:]] == ['base64', 'base64']
    assert content[1]['source']['media_type'] == 'image/png' and content[2]['source']['data'] == PNG
    assert 'client_composed' not in client.messages[-1]
    [user] = items_of(ph, turn, 'userMessage')
    assert user['clientId'] == 'client-1'
    assert user['content'][1:] == [{'type': 'image', 'url': data_url}, {'type': 'localImage', 'path': str(picture)}]
    read = await ph.call('thread/read', {'threadId': tid, 'includeTurns': True})
    assert read['thread']['turns'][-1]['items'][0]['clientId'] == 'client-1'
    await ph.turn(tid, '/clear everything'); await ph.drain()
    assert client.messages[-1]['client_composed'] is True  # a leading slash is literal text
    await ph.call('claude/input/submit', {'threadId': tid, 'messageId': 'agent-1', 'source': {'kind': 'agent', 'id': 'other'},
                                          'eventType': 'agent_message', 'text': 'read @/etc/passwd'})
    await ph.drain()
    assert client.messages[-1]['client_composed'] is True
    models = (await ph.call('model/list'))['data']
    assert all('image' in m['inputModalities'] for m in models)


@pytest.mark.parametrize('item,feature', [({'type': 'audio', 'url': 'data:audio/wav;base64,AA=='}, 'input:audio')])
async def test_unsupported_input_items_fail_clearly(ph, item, feature):
    tid = await ph.thread()
    error = await ph.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'hi'}, item]}, error=True)
    assert error['code'] == -32602 and error['data']['feature'] == feature


async def test_unmanaged_skill_path_is_rejected_before_starting_a_turn(ph):
    tid = await ph.thread()
    error = await ph.call('turn/start', {'threadId': tid, 'input': [
        {'type': 'skill', 'name': 'x', 'path': str(ph.settings.workspace / 'unknown' / 'SKILL.md')}]}, error=True)
    assert error['code'] == -32602
    assert not ph.store.pending(tid)
    assert not ph.sdk.clients


async def test_oversized_or_wrong_type_images_are_refused(ph):
    tid = await ph.thread()
    big = 'data:image/png;base64,' + base64.b64encode(b'0' * (5 * 1024 * 1024 + 1)).decode()
    for url in [big, 'data:image/tiff;base64,' + PNG, 'http://example.com/a.png']:
        error = await ph.call('turn/start', {'threadId': tid, 'input': [{'type': 'image', 'url': url}]}, error=True)
        assert error['code'] == -32602


# ---- settings, plan mode and unsupported methods ------------------------------

async def test_desktop_permission_presets_map_to_claude_modes(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    client = ph.sdk.clients[0]
    presets = {
        'auto': ({'approvalPolicy': 'on-request', 'sandboxPolicy': {'type': 'workspaceWrite', 'writableRoots': [], 'networkAccess': True,
                  'excludeTmpdirEnvVar': False, 'excludeSlashTmp': False}}, 'acceptEdits', 'workspaceWrite'),
        'readOnly': ({'approvalPolicy': 'on-request', 'sandboxPolicy': {'type': 'readOnly', 'networkAccess': False}}, 'default', 'readOnly'),
        'full': ({'approvalPolicy': 'never', 'sandboxPolicy': {'type': 'dangerFullAccess'}}, 'bypassPermissions', 'dangerFullAccess'),
    }
    for patch, mode, sandbox in presets.values():
        await ph.call('thread/settings/update', {'threadId': tid, **patch})
        assert ph.server.approvals.mode(tid) == mode and client.options.permission_mode == mode
        settings = sent(ph, 'thread/settings/updated')[-1]['params']['threadSettings']
        assert settings['sandboxPolicy']['type'] == sandbox
        assert settings['approvalPolicy'] == patch['approvalPolicy']


async def test_plan_mode_toggle_with_desktop_mask(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    modes = (await ph.call('collaborationMode/list'))['data']
    assert {m['mode'] for m in modes} == {'default', 'plan'}
    await ph.call('thread/settings/update', {'threadId': tid, 'collaborationMode': {'mode': 'plan', 'settings': {
        'model': '', 'reasoning_effort': None, 'developer_instructions': None}}})
    assert ph.server.approvals.planning(tid)
    assert sent(ph, 'thread/settings/updated')[-1]['params']['threadSettings']['collaborationMode']['mode'] == 'plan'


async def test_codex_only_methods_answer_or_fail_typed(ph):
    tid = await ph.thread()
    assert await ph.call('thread/goal/get', {'threadId': tid}) == {'goal': None}
    assert await ph.call('thread/goal/clear', {'threadId': tid}) == {'cleared': False}
    assert await ph.call('thread/backgroundTerminals/list', {'threadId': tid, 'limit': 50}) == {'data': [], 'nextCursor': None}
    for method, feature in [('thread/goal/set', 'goals'), ('review/start', 'review'),
                            ('thread/memoryMode/set', 'memoryMode'), ('thread/rollback', 'historyRevert')]:
        error = await ph.call(method, {'threadId': tid}, error=True)
        assert error['code'] == -32601
        assert {key: error['data'][key] for key in ('unsupported', 'runtime', 'feature', 'method')} == {
            'unsupported': True, 'runtime': 'claude', 'feature': feature, 'method': method}
    features = (await ph.call('claude/capabilities'))['features']
    assert features['planChecklist'] and features['imageInput'] and not features['goals'] and features['turnSteer']
    # Owner steer without an active turn: the Codex refusal text cutex maps to turn_conflict.
    error = await ph.call('turn/steer', {'threadId': tid, 'input': [{'type': 'text', 'text': 'x'}]}, error=True)
    assert error['code'] == -32600 and error['message'].startswith('no active turn to steer')


async def test_rate_limit_summary_uses_primary_and_secondary_windows(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    owner = ph.engine.driver.owners[tid]
    await owner._event({'type': 'rate_limit_event', 'rate_limit_info': {
        'rate_limit_type': 'five_hour', 'status': 'allowed', 'utilization': 0.25, 'resets_at': 4102444800}})
    await owner._event({'type': 'rate_limit_event', 'rate_limit_info': {
        'rate_limit_type': 'seven_day', 'status': 'allowed', 'utilization': 0.5, 'resets_at': 4102444800}})
    summary = sent(ph, 'account/rateLimits/updated')[-1]['params']['rateLimits']
    assert summary['primary']['usedPercent'] == 25 and summary['primary']['windowDurationMins'] == 300
    assert summary['secondary']['usedPercent'] == 50 and summary['secondary']['windowDurationMins'] == 10080
