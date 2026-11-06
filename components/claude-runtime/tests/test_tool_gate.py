"""claude-r37: a paused agent holds its next tool call in the PreToolUse hook (tool_gate.py)."""
from __future__ import annotations

import asyncio
import pytest

from cute_claude.store import identifier
from cute_claude.tool_gate import ENDED, EXPIRED, HOOK_TIMEOUT
from test_persistent import ph, until  # noqa: F401  (fixture)
from test_questions_plan import begin
from test_background_subagents import replay


def gate_events(ph, tid):
    return [m['params'] for m in ph.out if m.get('method') == 'claude/toolGate/changed' and m['params']['threadId'] == tid]


def flags_sent(ph, tid):
    return [m['params']['status'].get('activeFlags') for m in ph.out
            if m.get('method') == 'thread/status/changed' and m['params']['threadId'] == tid]


def call(bridge, name='Read', tool_input=None, call_id=None, **extra):
    call_id = call_id or identifier()
    data = {'session_id': bridge.sid, 'tool_name': name, 'tool_input': tool_input or {'file_path': 'hello.txt'}, **extra}
    return call_id, asyncio.create_task(bridge.pre_tool(data, call_id, {}))


def denied(decision):
    return decision.get('hookSpecificOutput', {}).get('permissionDecisionReason') \
        if decision.get('hookSpecificOutput', {}).get('permissionDecision') == 'deny' else None


async def test_capability_rpc_and_long_hook_timeout(ph):
    tid, turn, bridge = await begin(ph)
    assert (await ph.call('claude/capabilities'))['toolGate'] == 'claude/toolGate/set'
    assert ph.out[0]['result']['toolGate'] == 'claude/toolGate/set'  # initialize
    [matcher] = ph.sdk.clients[0].options.hooks['PreToolUse']
    assert matcher.timeout == HOOK_TIMEOUT
    assert await ph.call('claude/toolGate/get', {'threadId': tid}) == {
        'threadId': tid, 'held': False, 'waiting': [], 'waitingCount': 0}
    assert 'boolean' in (await ph.call('claude/toolGate/set', {'threadId': tid, 'held': 'yes'}, error=True))['message']
    await ph.call('claude/toolGate/set', {'threadId': 'missing-thread', 'held': True}, error=True)
    # Setting the same state twice is a no-op.
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': True})
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': True})
    assert [e['held'] for e in gate_events(ph, tid)] == [True]


async def test_open_gate_does_not_wait_or_emit(ph):
    tid, turn, bridge = await begin(ph)
    _, task = call(bridge)
    assert denied(await asyncio.wait_for(task, 2)) is None
    assert gate_events(ph, tid) == []


async def test_paused_call_waits_before_it_starts_and_resumes_in_the_same_turn(ph):
    tid, turn, bridge = await begin(ph)
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': True})
    call_id, task = call(bridge)
    await until(lambda: ph.engine.tool_gate.waiting.get(tid))
    await asyncio.sleep(0.05)
    assert not task.done()
    assert call_id not in bridge.native.calls, 'a held call has no started item'
    snapshot = await ph.call('claude/toolGate/get', {'threadId': tid})
    assert snapshot['held'] is True and snapshot['waitingCount'] == 1
    [row] = snapshot['waiting']
    assert (row['callId'], row['tool'], row['turnId'], row['agentId']) == (call_id, 'Read', turn, None)
    assert 'pausedAtTool' in ph.server.active_flags(tid)
    assert ['pausedAtTool'] in flags_sent(ph, tid)
    # Resume: the same call continues in the same turn.
    assert (await ph.call('claude/toolGate/set', {'threadId': tid, 'held': False}))['held'] is False
    assert denied(await asyncio.wait_for(task, 2)) is None
    assert bridge.binding[0] == turn and call_id in bridge.native.calls
    assert ph.server.active_flags(tid) == [] and flags_sent(ph, tid)[-1] == []
    assert gate_events(ph, tid)[-1] == {'threadId': tid, 'held': False, 'waiting': [], 'waitingCount': 0}


async def test_parallel_calls_and_a_foreground_subagent_call_hold_and_release_together(ph):
    tid, turn, bridge = await begin(ph)
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': True})
    child = 'a0123456789abcdef'
    bridge.children[child] = bridge.binding
    tasks = [call(bridge, 'Grep', {'pattern': 'x'})[1], call(bridge, 'Glob', {'pattern': '*'})[1],
             call(bridge, 'Read', agent_id=child)[1]]
    await until(lambda: len(ph.engine.tool_gate.waiting.get(tid, {})) == 3)
    snapshot = ph.engine.tool_gate.snapshot(tid)
    assert sorted(r['tool'] for r in snapshot['waiting']) == ['Glob', 'Grep', 'Read']
    assert {r['agentId'] for r in snapshot['waiting']} == {None, child}
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': False})
    assert [denied(d) for d in await asyncio.wait_for(asyncio.gather(*tasks), 2)] == [None, None, None]


async def test_stop_cancels_the_held_call_and_never_reports_an_ended_turn_active(ph):
    tid, turn, bridge = await begin(ph)
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': True})
    _, task = call(bridge)
    await until(lambda: ph.engine.tool_gate.waiting.get(tid))
    sent = len(flags_sent(ph, tid))
    ph.store.db.execute("UPDATE turns SET state='interrupted' WHERE id=?", (turn,))
    task.cancel()  # Claude Code cancels the hook callback on interrupt (checked against 2.1.314).
    with pytest.raises(asyncio.CancelledError):
        await task
    assert ph.engine.tool_gate.waiting.get(tid) is None
    assert len(flags_sent(ph, tid)) == sent, 'no status for a turn that is no longer running'
    assert gate_events(ph, tid)[-1]['waitingCount'] == 0
    # The gate stays closed: the pause outlives the stopped turn.
    assert ph.engine.tool_gate.closed(tid)


async def test_turn_end_while_held_refuses_the_call(ph):
    tid, turn, bridge = await begin(ph)
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': True})
    _, task = call(bridge)
    await until(lambda: ph.engine.tool_gate.waiting.get(tid))
    bridge.revoke()
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': False})
    assert denied(await asyncio.wait_for(task, 2)) == ENDED


async def test_a_hold_answers_itself_before_the_hook_deadline(ph):
    tid, turn, bridge = await begin(ph)
    ph.engine.tool_gate.hold_limit = 0.05
    await ph.call('claude/toolGate/set', {'threadId': tid, 'held': True})
    _, task = call(bridge)
    assert denied(await asyncio.wait_for(task, 2)) == EXPIRED
    assert ph.engine.tool_gate.waiting.get(tid) is None


async def test_background_subagent_tools_wait_too(ph):
    run = await replay(ph, 'default', mode='default')
    await run.until(lambda e: e['kind'] == 'msg' and e['data'].get('type') == 'result')
    await run.until(lambda e: e['kind'] == 'hook:PreToolUse' and e['data'].get('agent_id'))
    agent = 'aa335f43da4fb68e1'
    assert agent in run.bridge.background
    await ph.call('claude/toolGate/set', {'threadId': run.tid, 'held': True})
    call_id = identifier()
    task = asyncio.create_task(run.bridge.pre_tool({'session_id': run.owner.session_id, 'agent_id': agent,
        'hook_event_name': 'PreToolUse', 'tool_name': 'Read', 'tool_input': {}}, call_id, {}))
    await until(lambda: ph.engine.tool_gate.waiting.get(run.tid))
    [row] = ph.engine.tool_gate.snapshot(run.tid)['waiting']
    assert (row['agentId'], row['turnId']) == (agent, None)
    assert call_id not in run.bridge.background_calls
    await ph.call('claude/toolGate/set', {'threadId': run.tid, 'held': False})
    assert await asyncio.wait_for(task, 2) == {}
    assert run.bridge.background_calls[call_id] == agent
