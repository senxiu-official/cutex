"""Claude Code's own notification turn (docs/BUSY_DELIVERY.md, "Claude Code's notification turn").

One test per stated behaviour. The frames after a background subagent's end are
the recorded ones (fixtures/background_subagent_probe.json, Claude Code 2.1.301):
task_updated, task_notification, background_tasks_changed, system/init, the
model's reply, a Result.
"""
from __future__ import annotations

import asyncio
import json

import cute_claude.sdk_runtime as sdk_runtime
from cute_claude.config import RUNTIME_LIMITS
from cute_claude.store import dump, identifier
from cute_claude.subscription import NO_TURN_TOOLS
from test_autonomous_turns import idle_events, started
from test_background_subagents import PROBE
from test_persistent import ph, until  # noqa: F401  (fixture)


def notification_turn(owner):
    """The recorded frames from the subagent's end to the notification turn's Result."""
    script = PROBE['default']
    start = next(i for i, e in enumerate(script) if e['kind'] == 'msg' and e['data'].get('subtype') == 'task_updated')
    sid = next(e['data']['session_id'] for e in script if e['kind'] == 'msg' and e['data'].get('session_id'))
    frames = [json.loads(json.dumps(e['data']).replace(sid, owner.session_id))
              for e in script[start:] if e['kind'] == 'msg']
    assert [f.get('subtype') or f['type'] for f in frames][:4] == ['task_updated', 'task_notification',
                                                                    'background_tasks_changed', 'init']
    assert frames[-1]['type'] == 'result'
    return frames


async def feed(client, frames):
    for frame in frames:
        await client.events.put(frame)


def main_tool(owner, name='Read'):
    return {'session_id': owner.session_id, 'tool_name': name, 'tool_input': {'file_path': 'a.txt'},
            'hook_event_name': 'PreToolUse'}


async def test_its_tools_are_denied_with_the_documented_message(ph):
    tid, client, owner = await started(ph)
    frames = notification_turn(owner)
    await feed(client, frames[:-1])  # The turn runs: no Result yet.
    await until(lambda: owner.autonomous_until is not None and idle_events(ph, 'assistant'))
    for name in ('Read', 'Bash', 'mcp__cutex__agent_tell', 'Agent'):
        decision = await owner.bridge.pre_tool(main_tool(owner, name), identifier(), {})
        assert decision['hookSpecificOutput']['permissionDecision'] == 'deny'
        assert decision['hookSpecificOutput']['permissionDecisionReason'] == NO_TURN_TOOLS
    assert NO_TURN_TOOLS.startswith('No active turn accepts tools.') and 'Reply in one line' in NO_TURN_TOOLS
    await feed(client, frames[-1:])
    await until(lambda: owner.autonomous_until is None)


async def test_its_text_is_not_a_reply_and_reaches_no_timeline(ph):
    tid, client, owner = await started(ph)
    await feed(client, notification_turn(owner))
    await until(lambda: idle_events(ph, 'result'))
    assert ph.store.db.execute('SELECT count(*) FROM turns WHERE thread_id=?', (tid,)).fetchone()[0] == 0
    assert 'The background agent finished' not in dump(ph.store.events(tid, 0, 10000))
    events = [m['params'] for m in ph.out if m.get('method') == 'claude/runtime/event']
    assert events and all(e['whileIdle'] and e['rawContentRetained'] is False for e in events)


async def test_the_model_is_told_in_the_system_prompt(ph):
    for phrase in ('"Claude Task"', 'cannot use tools and the user does not see it',
                   'reply in one line and use no tools', 'act on it then'):
        assert phrase in RUNTIME_LIMITS
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    assert RUNTIME_LIMITS in ph.sdk.clients[0].options.system_prompt['append']


async def test_ignoring_it_runs_nothing_and_an_input_still_waits(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    frames = notification_turn(owner)
    await feed(client, frames[:-1])
    await until(lambda: idle_events(ph, 'assistant'))
    # The model calls tools anyway: every one is denied, nothing is recorded or run.
    for _ in range(3):
        decision = await owner.bridge.pre_tool(main_tool(owner, 'Bash'), identifier(), {})
        assert decision['hookSpecificOutput']['permissionDecision'] == 'deny'
    assert ph.store.db.execute('SELECT count(*) FROM tool_calls').fetchone()[0] == 0
    turn = await ph.turn(tid, 'FOREGROUND')
    await until(lambda: len(owner.deferred) == 1)
    assert client.query_count == 0
    await feed(client, frames[-1:])
    await ph.drain()
    assert client.query_count == 1 and ph.store.turn(turn)['status'] == 'completed'


async def test_one_notification_opens_one_wait_window(ph):
    tid, client, owner = await started(ph)
    frames = notification_turn(owner)
    await feed(client, frames[:2])  # task_updated, task_notification
    await until(lambda: owner.autonomous_until is not None)
    opened = owner.autonomous_until
    await feed(client, frames[2:])
    await until(lambda: owner.autonomous_until is None)
    assert opened > 0 and len(idle_events(ph, 'result')) == 1


async def test_the_cutex_message_turn_runs_after_it_with_only_its_reply(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_START_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    frames = notification_turn(owner)
    await feed(client, frames[:2])
    await until(lambda: owner.autonomous_until is not None)
    # cutex's "Claude Task" message arrives right after the notification, before the turn's first frame.
    ph.store.enqueue(tid, 'ctask-1', {'source': {'kind': 'service', 'id': 'Claude Task'}, 'eventType': 'claude_task',
                                      'text': 'Background subagent finished', 'metadata': {}}, 10)
    ph.engine.wake(tid)
    await until(lambda: len(owner.deferred) == 1)
    assert client.query_count == 0
    await feed(client, frames[2:])
    await ph.drain()
    [turn] = [r[0] for r in ph.store.db.execute('SELECT id FROM turns WHERE thread_id=?', (tid,))]
    items = dump(ph.store.items(turn))
    assert client.query_count == 1 and ph.store.turn(turn)['status'] == 'completed'
    assert 'The background agent finished' not in items and '[FIXTURE]' in items


async def test_a_notification_during_a_turn_makes_the_next_input_wait(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_START_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    first = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    notification = notification_turn(owner)[1]
    await client.events.put(notification)
    await until(lambda: owner.notified_in_turn)
    await client.events.put({'type': 'assistant', 'session_id': owner.session_id, 'content': [{'type': 'text', 'text': 'done'}]})
    await client.events.put({'type': 'result', 'session_id': owner.session_id, 'subtype': 'success', 'is_error': False,
                             'terminal_reason': 'completed', 'num_turns': 1, 'usage': {}})
    await until(lambda: ph.store.turn(first)['status'] == 'completed')
    assert owner.autonomous_until is not None and not owner.notified_in_turn
    second = await ph.turn(tid, 'NEXT')
    await until(lambda: len(owner.deferred) == 1)
    assert client.query_count == 1
    # Claude Code's notification turn follows; its Result releases the input.
    await feed(client, notification_turn(owner)[3:])
    await ph.drain()
    assert client.query_count == 2 and ph.store.turn(second)['status'] == 'completed'
    assert 'The background agent finished' not in dump(ph.store.items(second))
