"""Native shell observation; no PowerShell/Bash process or model is executed."""
import asyncio
from dataclasses import replace
from types import SimpleNamespace
import pytest
from test_persistent import ph
from test_questions_plan import begin
from test_native_tools import ask_mode
from cute_claude.native_tools import OUTPUT_BYTES, command_output

@pytest.mark.parametrize('shell', ['Bash', 'PowerShell'])
async def test_native_shell_command_events(ph, shell):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    args = {'command': 'Write-Output hello' if shell == 'PowerShell' else 'printf hello'}
    await b.pre_tool({'session_id': b.sid, 'tool_name': shell, 'tool_input': args}, 'shell', {})
    item = ph.store.items(turn)[-1]
    assert item['type'] == 'commandExecution' and item['command'] == args['command']
    assert item['commandActions'] == [{'type': 'unknown', 'command': args['command']}]
    assert item['status'] == 'inProgress' and item['exitCode'] is None
    await b.native.post({'session_id': b.sid, 'hook_event_name': 'PostToolUse', 'tool_response': {'stdout': 'hello', 'stderr': 'warning'}}, 'shell', {})
    done = ph.store.items(turn)[-1]
    assert done['type'] == 'commandExecution' and done['status'] == 'completed'
    assert done['aggregatedOutput'] == 'hello\nwarning' and done['exitCode'] == 0
    assert isinstance(done['durationMs'], int)
    assert ph.store.db.execute('SELECT count(*) FROM tool_calls').fetchone()[0] == 0
    items = [m['params']['item'] for m in ph.out if m.get('method') in {'item/started', 'item/completed'} and m.get('params', {}).get('item', {}).get('id') == 'shell']
    assert [x['type'] for x in items] == ['commandExecution', 'commandExecution']

@pytest.mark.parametrize('result,success,declined,status,code', [
    ({'stdout': 'before interrupt', 'interrupted': True}, True, False, 'completed', None),
    ('Exit code 7\nfailed command', False, False, 'failed', 7),
    ('Permission declined.', False, True, 'declined', None),
])
async def test_powershell_outcomes(ph, result, success, declined, status, code):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    await b.native.start('PowerShell', {'command': 'Get-Item missing'}, 'shell')
    await b.native.finish('shell', result, success, declined=declined)
    item = ph.store.items(turn)[-1]
    assert item['status'] == status and item['exitCode'] == code and item['aggregatedOutput']

async def test_powershell_full_output_budget_and_background(ph):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    await b.native.start('PowerShell', {'command': 'Write-Output long'}, 'shell')
    await b.native.finish('shell', {'stdout': 'x' * 30000}, True)
    assert ph.store.items(turn)[-1]['aggregatedOutput'] == 'x' * 30000
    assert len(ph.store.items(turn)[-1]['aggregatedOutput']) <= OUTPUT_BYTES
    assert command_output('PowerShell', {'stdout': 'started', 'backgroundTaskId': 'task-1'}) == 'started\n[running in background as task task-1]'

async def test_powershell_disabled_remains_denied(ph):
    tid, turn, b = await begin(ph)
    before = ph.store.items(turn)
    denied = await b.native.start('PowerShell', {'command': 'Write-Output denied'}, 'denied')
    assert denied['hookSpecificOutput']['permissionDecision'] == 'deny'
    assert ph.store.items(turn) == before

async def test_powershell_approval_uses_original_command(ph):
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    ask_mode(ph, tid)
    args = {'command': 'Write-Output approval-test'}
    await b.native.start('PowerShell', args, 'shell')
    task = asyncio.create_task(b.native.approve('PowerShell', args, SimpleNamespace(tool_use_id='shell')))
    try:
        async with asyncio.timeout(2):
            while not ph.server.approvals.pending: await asyncio.sleep(.001)
        req = next(x for x in ph.out if x.get('method') == 'item/commandExecution/requestApproval')
        assert req['params']['command'] == args['command']
        await ph.server.handle(ph.peer, {'id': req['id'], 'result': {'decision': 'accept'}})
        assert (await task)['allow']
    finally:
        if not task.done(): task.cancel(); await asyncio.gather(task, return_exceptions=True)

async def test_powershell_background_registration_uses_shell_lifecycle(ph):
    from unittest.mock import AsyncMock
    ph.engine.settings = replace(ph.settings, allow_exec=True)
    tid, turn, b = await begin(ph)
    owners = ph.engine.driver.owners
    previous = owners.get(tid)
    started = AsyncMock()
    owners[tid] = SimpleNamespace(tasks=SimpleNamespace(shell_started=started))
    args = {'command': 'Start-Sleep 5', 'run_in_background': True}
    try:
        await b.native.start('PowerShell', args, 'shell')
        await b.native.finish('shell', {'stdout': '', 'backgroundTaskId': 'task-ps'}, True)
        started.assert_awaited_once_with('task-ps', turn, 'shell', args)
    finally:
        if previous is None: owners.pop(tid, None)
        else: owners[tid] = previous
