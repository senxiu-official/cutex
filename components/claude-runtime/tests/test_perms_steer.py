"""Owner steer through PostToolUse, persistent allow rules and live permission modes.

Offline only: the fake SDK stands in for Claude Code; no model, login or network.
"""
import asyncio
import base64
import json
from dataclasses import replace
from types import SimpleNamespace

import pytest

from cute_claude import permission_rules
from cute_claude.busy_delivery import STEER_FRAME
from test_persistent import ph  # noqa: F401
from test_questions_plan import begin
from test_desktop_parity import ask_mode, sent, wait_request

PNG = base64.b64encode(b'\x89PNG\r\n\x1a\n' + b'0' * 64).decode()


def steer(tid, text='also check the README', client='c-1', **extra):
    return {'threadId': tid, 'input': [{'type': 'text', 'text': text, 'text_elements': []}],
            'clientUserMessageId': client, **extra}


async def post_hook(ph, bridge, call='tool-1'):
    data = {'session_id': bridge.sid, 'hook_event_name': 'PostToolUse', 'tool_name': 'Read',
            'tool_input': {'file_path': str(ph.settings.workspace / 'hello.txt')}, 'tool_response': 'ok'}
    await bridge.pre_tool(data, call, {})
    hook = ph.sdk.clients[0].options.hooks['PostToolUse'][0].hooks[0]
    return await hook(data, call, {})


# ---- steer ----------------------------------------------------------------------

async def test_capabilities_advertise_steer_and_live_modes(ph):
    caps = await ph.call('claude/capabilities')
    assert caps['input']['sameTurnSteering'] is True
    assert caps['features']['turnSteer'] is True and caps['features']['permissionRules'] is True
    assert caps['features']['claudePermissionModes'] == ['default', 'acceptEdits', 'plan', 'auto', 'bypassPermissions']
    assert caps['tools']['livePermissionMode'] is True


async def test_steer_is_read_at_the_next_tool_call_and_shown_in_the_running_turn(ph):
    tid, turn, bridge = await begin(ph)
    result = await ph.call('turn/steer', steer(tid, expectedTurnId=turn))
    assert result == {'turnId': turn}
    context = (await post_hook(ph, bridge))['hookSpecificOutput']['additionalContext']
    assert context == STEER_FRAME + 'also check the README'
    await asyncio.sleep(0)  # the user message is published right after the hook returns
    await asyncio.sleep(0)
    shown = [m['params']['item'] for m in sent(ph, 'item/completed') if m['params']['item']['type'] == 'userMessage']
    assert shown and shown[-1]['clientId'] == 'c-1' and shown[-1]['steered'] is True
    assert shown[-1]['content'][0]['text'] == 'also check the README'
    assert any(i['type'] == 'userMessage' and i['clientId'] == 'c-1' for i in ph.store.items(turn))
    before = len(sent(ph, 'item/completed'))
    await ph.server.busy_delivery.acknowledge(tid, turn)
    assert len(sent(ph, 'item/completed')) == before, 'acknowledgement does not show the message twice'
    assert sent(ph, 'claude/input/contextPersisted')[-1]['params']['messageId'] == 'steer-c-1'
    ph.store.finish(tid, turn, 'completed')
    assert ph.store.claim(tid) is None, 'a steered message never starts another turn'


async def test_steer_without_a_tool_call_runs_as_the_next_turn(ph):
    tid, turn, bridge = await begin(ph)
    await ph.call('turn/steer', steer(tid))
    ph.store.finish(tid, turn, 'completed')
    following = ph.store.claim(tid)
    assert following['message_id'] == 'steer-c-1' and following['turn_id'] != turn
    first = ph.store.items(following['turn_id'])[0]
    assert first['type'] == 'userMessage' and first['clientId'] == 'c-1'


async def test_unread_steer_is_dropped_when_its_turn_is_interrupted(ph):
    # Owner report: a steer sent while a tool ran, then Esc (interrupt & send). The turn never read
    # it; the desktop resent it as a new turn, and the stale steer was injected into that turn at
    # its first tool call too: the model got the message twice, the timeline showed it twice.
    tid, turn, bridge = await begin(ph)
    await ph.call('turn/steer', steer(tid, expectedTurnId=turn))
    await bridge.end_turn()
    ph.store.finish(tid, turn, 'interrupted')
    state = ph.store.db.execute("SELECT state FROM inputs WHERE message_id='steer-c-1'").fetchone()[0]
    assert state == 'interrupted'
    # The client resends it under a new id; that turn reads no stale steer at its tool calls.
    ph.store.db.execute('UPDATE threads SET halted=0 WHERE id=?', (tid,))
    ph.store.enqueue(tid, 'resend', {'source': {'kind': 'user', 'id': 'test'}, 'eventType': 'user.message',
                                     'text': 'also check the README', 'clientId': 'c-2', 'metadata': {}}, 10)
    following = ph.store.claim(tid)
    assert following['message_id'] == 'resend'
    bridge.begin_turn(following['turn_id'])
    hook = await post_hook(ph, bridge, call='tool-2')
    assert STEER_FRAME not in json.dumps(hook)
    assert not any(i.get('steered') for i in ph.store.items(following['turn_id']))
    ph.store.finish(tid, following['turn_id'], 'completed')
    assert ph.store.claim(tid) is None, 'the dropped steer never runs as a turn of its own either'


async def test_unread_steer_survives_a_completed_turn(ph):
    tid, turn, bridge = await begin(ph)
    await ph.call('turn/steer', steer(tid))
    ph.store.finish(tid, turn, 'completed')
    assert ph.store.claim(tid)['message_id'] == 'steer-c-1'


async def until(predicate):
    async with asyncio.timeout(5):
        while not predicate():
            await asyncio.sleep(0.005)


async def running_turn(ph, tid=None):
    """A real HANG turn through the fake SDK: its bridge, as Claude Code drives it."""
    tid = tid or await ph.thread()
    turn = await ph.call('turn/start', {'threadId': tid, 'clientUserMessageId': 'first',
                                        'input': [{'type': 'text', 'text': 'HANG', 'text_elements': []}]})
    turn = turn['turn']['id']
    await until(lambda: tid in ph.engine.driver.owners and ph.engine.driver.owners[tid].bridge
                and ph.engine.driver.owners[tid].bridge.binding and ph.engine.driver.owners[tid].bridge.binding[0] == turn)
    return tid, turn, ph.engine.driver.owners[tid].bridge


def steered_items(ph, tid):
    """Steer hook deliveries recorded in this thread's turns (one per hook frame)."""
    return ph.store.db.execute("SELECT count(*) FROM items i JOIN turns t ON t.id=i.turn_id WHERE t.thread_id=? "
                               "AND json_extract(i.payload,'$.steered')=1", (tid,)).fetchone()[0]


async def test_interrupt_then_send_delivers_the_text_once(ph):
    # Owner notes #6 / #11 (claude-r16): interrupt & send. The steer the stopped turn never read
    # arrived as the new turn's user message AND, at that turn's next tool call, as hook context.
    tid, turn, _ = await running_turn(ph)
    await ph.call('turn/steer', steer(tid, expectedTurnId=turn))
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert ph.store.turn(turn)['status'] == 'interrupted'
    assert ph.store.db.execute("SELECT state FROM inputs WHERE message_id='steer-c-1'").fetchone()[0] == 'interrupted'
    # The desktop resends the unread steer (merged with any new text) under a new client id; here
    # a HANG text stands in for it so the resent turn stays open for tool calls.
    following = await ph.call('turn/start', {'threadId': tid, 'clientUserMessageId': 'c-2',
        'input': [{'type': 'text', 'text': 'HANG', 'text_elements': []}]})
    following = following['turn']['id']
    bridge = ph.engine.driver.owners[tid].bridge
    await until(lambda: bridge.binding and bridge.binding[0] == following)
    for call in ('tool-2', 'tool-3'):
        assert STEER_FRAME not in json.dumps(await post_hook(ph, bridge, call=call))
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': following})
    assert steered_items(ph, tid) == 0
    assert ph.store.claim(tid) is None, 'nor does it run as a turn of its own afterwards'


async def test_steer_while_busy_without_interrupt_is_read_once_by_the_hook(ph):
    tid, turn, bridge = await running_turn(ph)
    await ph.call('turn/steer', steer(tid, expectedTurnId=turn))
    first = await post_hook(ph, bridge, call='tool-1')
    assert first['hookSpecificOutput']['additionalContext'] == STEER_FRAME + 'also check the README'
    assert STEER_FRAME not in json.dumps(await post_hook(ph, bridge, call='tool-2'))
    # An interrupt after the hook read it keeps it read (the desktop sees it recorded, no resend).
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert ph.store.claim(tid) is None
    assert steered_items(ph, tid) == 1


async def test_tool_boundary_while_the_interrupt_is_in_flight_does_not_read_the_steer(ph):
    # The race: the owner pressed Esc; before Claude Code aborts, one more PostToolUse runs. The
    # steer is the client's to resend now, so the hook must not deliver it as well.
    tid, turn, bridge = await running_turn(ph)
    await ph.call('turn/steer', steer(tid, expectedTurnId=turn))
    real = ph.engine.driver.interrupt
    seen = []
    async def late_hook_then_interrupt(t, turn_id):
        seen.append(await post_hook(ph, bridge, call='tool-late'))
        return await real(t, turn_id)
    ph.engine.driver.interrupt = late_hook_then_interrupt
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert seen and STEER_FRAME not in json.dumps(seen[0])
    assert not any(i.get('steered') for i in ph.store.items(turn))


async def test_unread_steer_does_not_run_after_an_interrupted_turn_that_completes_anyway(ph):
    # The interrupt lost the race to the terminal result: the turn completes. The owner still asked
    # to stop and the client resends the steer, so the FIFO fallback must not run it as well.
    tid, turn, bridge = await running_turn(ph)
    await ph.call('turn/steer', steer(tid, expectedTurnId=turn))
    async def completes_anyway(t, turn_id):
        return True
    ph.engine.driver.interrupt = completes_anyway
    task = asyncio.create_task(ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn}))
    await asyncio.sleep(0.05)
    assert ph.store.db.execute("SELECT state FROM inputs WHERE message_id='steer-c-1'").fetchone()[0] == 'interrupted'
    ph.engine.workers[tid].cancel()
    await asyncio.gather(task, return_exceptions=True)
    assert ph.store.claim(tid) is None


async def test_a_failed_interrupt_request_keeps_the_steer_waiting(ph):
    tid, turn, bridge = await running_turn(ph)
    await ph.call('turn/steer', steer(tid, expectedTurnId=turn))
    async def refused(t, turn_id):
        raise RuntimeError('interrupt not delivered')
    ph.engine.driver.interrupt = refused
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn}, error=True)
    assert ph.store.db.execute("SELECT state FROM inputs WHERE message_id='steer-c-1'").fetchone()[0] == 'queued'
    hook = await post_hook(ph, bridge, call='tool-1')
    assert hook['hookSpecificOutput']['additionalContext'] == STEER_FRAME + 'also check the README'


async def test_unread_image_steer_is_dropped_when_its_turn_is_interrupted(ph):
    tid, turn, _ = await running_turn(ph)
    params = steer(tid)
    params['input'].append({'type': 'image', 'url': f'data:image/png;base64,{PNG}'})
    await ph.call('turn/steer', params)
    await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
    assert ph.store.db.execute("SELECT state FROM inputs WHERE message_id='steer-c-1'").fetchone()[0] == 'interrupted'
    assert ph.store.claim(tid) is None, 'the client resends it; it never runs as a turn of its own too'


async def test_steer_retry_with_the_same_client_id_is_one_input(ph):
    tid, turn, _ = await begin(ph)
    await ph.call('turn/steer', steer(tid))
    await ph.call('turn/steer', steer(tid))
    rows = ph.store.db.execute("SELECT count(*) FROM inputs WHERE message_id='steer-c-1'").fetchone()[0]
    assert rows == 1


async def test_steer_refusals_use_codex_texts(ph):
    tid, turn, _ = await begin(ph)
    error = await ph.call('turn/steer', steer(tid, expectedTurnId='other'), error=True)
    assert error['code'] == -32600 and error['message'].startswith('expected active turn id')
    ph.store.finish(tid, turn, 'completed')
    error = await ph.call('turn/steer', steer(tid, client='c-2'), error=True)
    assert error['message'].startswith('no active turn to steer')


async def test_steer_with_an_image_waits_for_the_next_turn(ph):
    tid, turn, bridge = await begin(ph)
    params = steer(tid)
    params['input'].append({'type': 'image', 'url': f'data:image/png;base64,{PNG}'})
    await ph.call('turn/steer', params)
    assert ph.store.db.execute('SELECT count(*) FROM busy_inputs').fetchone()[0] == 0
    assert 'hookSpecificOutput' not in (await post_hook(ph, bridge))
    ph.store.finish(tid, turn, 'completed')
    assert ph.store.claim(tid)['message_id'] == 'steer-c-1'


async def test_mock_backend_does_not_steer(h):
    tid = await h.thread()
    error = await h.call('turn/steer', {'threadId': tid, 'input': []}, error=True)
    assert error['code'] == -32601 and error['data']['feature'] == 'turnSteer'
    features = (await h.call('claude/capabilities'))['features']
    assert features['turnSteer'] is False and features['permissionRules'] is False


# ---- persistent allow rules ------------------------------------------------------

def test_rule_syntax_suggestions_and_fallbacks(tmp_path):
    assert permission_rules.valid_rule('Bash(git status:*)') and permission_rules.valid_rule('Edit')
    assert permission_rules.valid_rule('mcp__cutex__read_file')
    for bad in ['', 'Bash(', 'Bash()', 'bad name', 'Bash(x)\n', 7, '(x)']:
        assert not permission_rules.valid_rule(bad)
    suggestions = [SimpleNamespace(type='addRules', behavior='allow', destination='localSettings',
                                   rules=[SimpleNamespace(tool_name='Bash', rule_content='git status:*')]),
                   {'type': 'setMode', 'mode': 'acceptEdits'},
                   {'type': 'addRules', 'behavior': 'deny', 'rules': [{'toolName': 'Bash', 'ruleContent': 'rm:*'}]}]
    assert permission_rules.from_suggestions(suggestions) == ['Bash(git status:*)']
    work = tmp_path / 'w'
    (work / 'src').mkdir(parents=True)
    assert permission_rules.fallback('Bash', {'command': ' npm test '}, work) == ['Bash(npm test)']
    assert permission_rules.fallback('Write', {'file_path': str(work / 'src' / 'a.py')}, work) == ['Edit(src/**)']
    assert permission_rules.fallback('Edit', {'file_path': str(work / 'a.py')}, work) == ['Edit(**)']
    assert permission_rules.fallback('Edit', {'file_path': '/elsewhere/a.py'}, work) == []
    assert permission_rules.fallback('WebFetch', {'url': 'https://x'}, work) == ['WebFetch']


def test_rules_file_and_flag_settings(tmp_path):
    assert permission_rules.load(tmp_path) == []
    (tmp_path / permission_rules.RULES_FILE).write_text(json.dumps({'allow': ['Bash(ls:*)', 'bad name', 'Bash(ls:*)', 'Edit(src/**)']}))
    assert permission_rules.load(tmp_path) == ['Bash(ls:*)', 'Edit(src/**)']
    assert permission_rules.flag_settings(None, []) is None
    merged = json.loads(permission_rules.flag_settings({'apiKeyHelper': 'helper'}, ['Bash(ls:*)']))
    assert merged == {'apiKeyHelper': 'helper', 'permissions': {'allow': ['Bash(ls:*)']}}
    # Merged with the other flag settings (fast mode, an account's own permissions), never replacing them.
    base = {'fastMode': True, 'permissions': {'deny': ['Read(.env)'], 'allow': ['WebSearch']}}
    assert permission_rules.with_rules(base, ['Bash(ls:*)']) == {
        'fastMode': True, 'permissions': {'deny': ['Read(.env)'], 'allow': ['WebSearch', 'Bash(ls:*)']}}
    assert permission_rules.with_rules({'fastMode': True}, []) == {'fastMode': True}
    assert permission_rules.with_rules({}, [], explicit=True) == {'permissions': {'allow': []}}


async def test_runtime_start_passes_persisted_rules_as_flag_settings(ph):
    ph.settings.state_dir.mkdir(parents=True, exist_ok=True)
    (ph.settings.state_dir / permission_rules.RULES_FILE).write_text(json.dumps({'allow': ['Bash(git status:*)']}))
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert json.loads(ph.sdk.clients[-1].options.settings) == {'permissions': {'allow': ['Bash(git status:*)']}, 'disableAllHooks': True}


async def test_always_allow_in_project_offers_rules_and_adds_the_chosen_ones_to_the_session(ph):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    ask_mode(ph, tid)
    args = {'command': 'git status --short'}
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Bash', 'tool_input': args}, 'b', {})
    context = SimpleNamespace(tool_use_id='b', suggestions=[SimpleNamespace(
        type='addRules', behavior='allow', destination='localSettings',
        rules=[SimpleNamespace(tool_name='Bash', rule_content='git status:*')])])
    task = asyncio.create_task(ph.sdk.clients[0].options.can_use_tool('Bash', args, context))
    request = await wait_request(ph, 'item/commandExecution/requestApproval')
    assert request['params']['claudePermissionRules'] == ['Bash(git status:*)']
    assert {'acceptWithClaudeRules': {'rules': ['Bash(git status:*)']}} in request['params']['availableDecisions']
    await ph.server.handle(ph.peer, {'id': request['id'], 'result': {'decision': {
        'acceptWithClaudeRules': {'rules': ['Bash(git status:*)']}}}})
    result = await asyncio.wait_for(task, 2)
    assert result.behavior == 'allow'
    [update] = result.updated_permissions
    assert (update.type, update.behavior, update.destination) == ('addRules', 'allow', 'session')
    assert [(r.tool_name, r.rule_content) for r in update.rules] == [('Bash', 'git status:*')]


async def test_a_rule_that_was_not_offered_only_accepts_once(ph):
    ph.engine.settings = replace(ph.settings, allow_write=True)
    tid, turn, b = await begin(ph)
    ask_mode(ph, tid)
    target = ph.settings.workspace / 'a.txt'
    args = {'file_path': str(target), 'content': 'y\n'}
    await b.pre_tool({'session_id': b.sid, 'tool_name': 'Write', 'tool_input': args}, 'w', {})
    task = asyncio.create_task(ph.sdk.clients[0].options.can_use_tool('Write', args, SimpleNamespace(tool_use_id='w')))
    request = await wait_request(ph, 'item/fileChange/requestApproval')
    assert request['params']['claudePermissionRules'] == ['Edit(**)']
    await ph.server.handle(ph.peer, {'id': request['id'], 'result': {'decision': {
        'acceptWithClaudeRules': {'rules': ['Bash(*)']}}}})
    result = await asyncio.wait_for(task, 2)
    assert result.behavior == 'allow' and result.updated_permissions is None


async def test_reload_applies_rules_to_a_live_runtime_or_at_next_start(ph):
    tid = await ph.thread()
    assert (await ph.call('claude/permissionRules/reload', {'threadId': tid}))['applied'] == 'nextStart'
    await ph.call('claude/runtime/start', {'threadId': tid})
    (ph.settings.state_dir / permission_rules.RULES_FILE).write_text(json.dumps({'allow': ['Edit(src/**)']}))
    client = ph.sdk.clients[-1]
    controls = []

    async def control(request):
        controls.append(request)
        return {}
    client._query = SimpleNamespace(_send_control_request=control)
    result = await ph.call('claude/permissionRules/reload', {'threadId': tid})
    assert result == {'threadId': tid, 'rules': 1, 'applied': 'live'}
    assert controls == [{'subtype': 'apply_flag_settings', 'settings': {'disableAllHooks': True, 'permissions': {'allow': ['Edit(src/**)']}}}]
    (ph.settings.state_dir / permission_rules.RULES_FILE).write_text(json.dumps({'allow': []}))
    await ph.call('claude/permissionRules/reload', {'threadId': tid})
    assert controls[-1]['settings'] == {'disableAllHooks': True, 'permissions': {'allow': []}}, 'removing the last rule clears the live list'

    async def refuse(request):
        raise RuntimeError('unsupported control request')
    client._query = SimpleNamespace(_send_control_request=refuse)
    assert (await ph.call('claude/permissionRules/reload', {'threadId': tid}))['applied'] == 'nextStart'


# ---- permission modes ------------------------------------------------------------

@pytest.mark.parametrize('patch,mode', [
    ({'approvalPolicy': 'on-request', 'approvalsReviewer': 'user', 'sandboxPolicy': {'type': 'readOnly'}}, 'default'),
    ({'approvalPolicy': 'on-request', 'approvalsReviewer': 'user', 'sandboxPolicy': {'type': 'workspaceWrite'}}, 'acceptEdits'),
    ({'approvalPolicy': 'on-request', 'approvalsReviewer': 'auto_review', 'sandboxPolicy': {'type': 'workspaceWrite'}}, 'auto'),
    ({'approvalPolicy': 'never', 'sandboxPolicy': {'type': 'dangerFullAccess'}}, 'bypassPermissions'),
])
async def test_desktop_mode_patches_reach_a_running_turn(ph, patch, mode):
    tid, turn, _ = await begin(ph)
    if mode == 'bypassPermissions':
        ask_mode(ph, tid)
    await ph.call('thread/settings/update', {'threadId': tid, **patch})
    assert ph.server.approvals.mode(tid) == mode
    assert ph.sdk.clients[-1].options.permission_mode == mode
    await ph.call('thread/settings/update', {'threadId': tid, 'collaborationMode': {'mode': 'plan', 'settings': {
        'model': ph.store.thread(tid)['model'], 'reasoning_effort': None, 'developer_instructions': None}}})
    assert ph.sdk.clients[-1].options.permission_mode == 'plan'
    settings = sent(ph, 'thread/settings/updated')[-1]['params']['threadSettings']
    assert settings['collaborationMode']['mode'] == 'plan'


def test_external_mcp_tool_offers_its_own_tool_rule(tmp_path):
    # Host-selected MCP servers (Claude Code sources) go through the normal approval path; without a
    # runtime suggestion the project rule is the exact MCP tool, never the whole server.
    from types import SimpleNamespace
    offered = permission_rules.offered('mcp__github__create_issue', {'title': 'x'}, SimpleNamespace(suggestions=[]), tmp_path)
    assert offered == ['mcp__github__create_issue']
    assert permission_rules.valid_rule('mcp__claude_ai_Gmail__search')


# ---- starting mode (cutex launch.claudePermissionMode) ----------------------------

@pytest.mark.parametrize('patch,mode', [
    ({'approvalPolicy': 'on-request', 'approvalsReviewer': 'user', 'sandboxPolicy': {'type': 'readOnly', 'networkAccess': False}}, 'default'),
    ({'approvalPolicy': 'on-request', 'approvalsReviewer': 'user', 'sandboxPolicy': {'type': 'workspaceWrite', 'writableRoots': [], 'networkAccess': True, 'excludeTmpdirEnvVar': False, 'excludeSlashTmp': False}}, 'acceptEdits'),
    ({'approvalPolicy': 'on-request', 'approvalsReviewer': 'auto_review', 'sandboxPolicy': {'type': 'workspaceWrite', 'writableRoots': [], 'networkAccess': True, 'excludeTmpdirEnvVar': False, 'excludeSlashTmp': False}}, 'auto'),
    ({'collaborationMode': {'mode': 'plan', 'settings': {'model': '', 'reasoning_effort': None, 'developer_instructions': None}}}, 'plan'),
])
async def test_cutex_starting_mode_before_the_first_runtime_start(ph, patch, mode):
    # cutex: claude/thread/ensure, then thread/settings/update, then claude/runtime/start.
    tid = '0b6f5a52-2a4e-4c3e-9a51-7d0c2f1e9a10'
    assert (await ph.call('claude/thread/ensure', {'threadId': tid}))['created'] is True
    await ph.call('thread/settings/update', {'threadId': tid, **patch})
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert ph.sdk.clients[-1].options.permission_mode == mode
    if mode == 'plan':
        assert ph.server.approvals.mode(tid) == 'bypassPermissions', 'plan keeps the default mode underneath'
