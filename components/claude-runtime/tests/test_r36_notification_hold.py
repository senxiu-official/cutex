"""claude-r36: the start window after a turn holds only for a background task's notification.

docs/BUSY_DELIVERY.md, "Autonomous turns". Claude Code 2.1.314 also sends
``task_notification`` for a foreground Bash command that ran 2 s or more
(registered as ``local_bash`` with ``is_backgrounded: false``); its result goes
back to the model in the running turn, so no notification turn follows. A
background task's notification that Claude Code folds into a later request of
the same turn (it drains queued notifications after a tool round) starts no turn
either. Only a background notification after the turn's last tool round holds
the next input for the start window.
"""
from __future__ import annotations

import asyncio

import cute_claude.sdk_runtime as sdk_runtime
from cute_claude.store import dump
from test_autonomous_turns import started
from test_notification_turn import feed, notification_turn
from test_persistent import ph, until  # noqa: F401  (fixture)


def task_started(owner, task_id, backgrounded, task_type='local_bash'):
    return {'type': 'system', 'subtype': 'task_started', 'session_id': owner.session_id, 'uuid': f'u-{task_id}',
            'task_id': task_id, 'tool_use_id': f'toolu_{task_id}', 'description': 'sleep 3', 'task_type': task_type,
            'is_backgrounded': backgrounded}


def task_notification(owner, task_id):
    return {'type': 'system', 'subtype': 'task_notification', 'session_id': owner.session_id, 'uuid': f'n-{task_id}',
            'task_id': task_id, 'tool_use_id': f'toolu_{task_id}', 'status': 'completed', 'output_file': '',
            'summary': 'sleep 3'}


def tool_result(owner, task_id):
    return {'type': 'user', 'session_id': owner.session_id, 'parent_tool_use_id': None, 'uuid': f'r-{task_id}',
            'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': f'toolu_{task_id}',
                                                     'content': 'done'}]}}


def message_start(owner, message_id):
    return {'type': 'stream_event', 'session_id': owner.session_id, 'uuid': f's-{message_id}',
            'parent_tool_use_id': None, 'event': {'type': 'message_start', 'message': {'id': message_id}}}


def reply(owner, text='done'):
    return {'type': 'assistant', 'session_id': owner.session_id, 'content': [{'type': 'text', 'text': text}]}


def result(owner):
    return {'type': 'result', 'session_id': owner.session_id, 'subtype': 'success', 'is_error': False,
            'terminal_reason': 'completed', 'num_turns': 1, 'usage': {}}


async def finish(ph, client, owner, turn):
    await feed(client, [reply(owner), result(owner)])
    await until(lambda: ph.store.turn(turn)['status'] == 'completed')


async def next_input_runs_at_once(ph, client, owner, tid):
    turn = await ph.turn(tid, 'NEXT')
    await until(lambda: client.query_count == 2)
    assert owner.deferred == [] and owner.autonomous_until is None
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed'
    held = [m for m in ph.out if m.get('method') == 'thread/status/changed'
            and 'heldByNotification' in (m['params']['status'].get('activeFlags') or [])]
    assert held == []


async def test_a_foreground_task_notification_does_not_hold(ph):
    tid, client, owner = await started(ph)
    first = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    # A foreground Bash command that ran 2 s: started, notified when it ends, then its tool result.
    await feed(client, [task_started(owner, 'fg1', False), task_notification(owner, 'fg1')])
    await until(lambda: owner.tasks.rows.get('fg1', {}).get('_backgrounded') is False)
    await asyncio.sleep(0.05)
    assert not owner.notified_in_turn
    await finish(ph, client, owner, first)
    assert owner.autonomous_until is None
    await next_input_runs_at_once(ph, client, owner, tid)


async def test_a_background_notification_followed_by_a_model_frame_does_not_hold(ph):
    tid, client, owner = await started(ph)
    first = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    await feed(client, [task_started(owner, 'bg1', True), task_notification(owner, 'bg1')])
    await until(lambda: owner.notified_in_turn)
    # The turn's next tool round ends; the request after it carries the notification.
    await feed(client, [tool_result(owner, 'other')])
    await until(lambda: owner.notice_drained)
    assert owner.notified_in_turn
    await feed(client, [message_start(owner, 'msg_2')])
    await until(lambda: not owner.notified_in_turn)
    await finish(ph, client, owner, first)
    assert owner.autonomous_until is None
    await next_input_runs_at_once(ph, client, owner, tid)


async def test_a_model_frame_without_a_tool_round_keeps_the_hold(ph):
    """Frames of the request already streaming when the notification came do not carry it."""
    tid, client, owner = await started(ph)
    first = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    await feed(client, [message_start(owner, 'msg_1'), task_started(owner, 'bg2', True), task_notification(owner, 'bg2'),
                        {'type': 'stream_event', 'session_id': owner.session_id, 'uuid': 's-delta',
                         'parent_tool_use_id': None, 'event': {'type': 'content_block_delta', 'index': 0,
                                                               'delta': {'type': 'text_delta', 'text': 'x'}}}])
    await until(lambda: owner.notified_in_turn)
    # A replayed user message (no tool result) is no tool round either.
    await feed(client, [{'type': 'user', 'session_id': owner.session_id, 'parent_tool_use_id': None, 'uuid': 'echo',
                         'message': {'role': 'user', 'content': [{'type': 'text', 'text': 'inserted'}]}}])
    await finish(ph, client, owner, first)
    assert owner.autonomous_until is not None and not owner.notified_in_turn


async def test_a_background_notification_after_the_last_frame_holds_the_start_window(ph):
    assert sdk_runtime.AUTONOMOUS_START_SECONDS == 10.0
    tid, client, owner = await started(ph)
    first = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    # A tool round, then the final reply, then the background command's notification.
    await feed(client, [task_started(owner, 'bg3', True), tool_result(owner, 'bg3'), reply(owner, 'started it'),
                        task_notification(owner, 'bg3')])
    await until(lambda: owner.notified_in_turn)
    assert not owner.notice_drained
    await feed(client, [result(owner)])
    await until(lambda: ph.store.turn(first)['status'] == 'completed')
    left = owner.autonomous_until - asyncio.get_running_loop().time()
    assert 9.0 < left <= 10.0
    second = await ph.turn(tid, 'NEXT')
    await until(lambda: len(owner.deferred) == 1)
    await until(lambda: any(m.get('method') == 'thread/status/changed'
                            and 'heldByNotification' in (m['params']['status'].get('activeFlags') or [])
                            for m in ph.out))
    assert client.query_count == 1
    # Claude Code's notification turn follows; its Result releases the input, which owns only its reply.
    await feed(client, notification_turn(owner)[3:])
    await ph.drain()
    assert client.query_count == 2 and ph.store.turn(second)['status'] == 'completed'
    assert 'The background agent finished' not in dump(ph.store.items(second))


async def test_a_foreground_task_moved_to_the_background_holds(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_START_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    first = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    await feed(client, [task_started(owner, 'fg2', False),
                        {'type': 'system', 'subtype': 'task_updated', 'session_id': owner.session_id,
                         'task_id': 'fg2', 'patch': {'is_backgrounded': True}},
                        task_notification(owner, 'fg2')])
    await until(lambda: owner.notified_in_turn)
    assert owner.tasks.rows['fg2']['_backgrounded'] is True
    await finish(ph, client, owner, first)
    assert owner.autonomous_until is not None


async def test_an_unknown_task_notification_holds(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_START_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    first = await ph.turn(tid, 'HANG')
    await until(lambda: client.query_count == 1)
    await feed(client, [task_notification(owner, 'never-started')])
    await until(lambda: owner.notified_in_turn)
    await finish(ph, client, owner, first)
    assert owner.autonomous_until is not None
