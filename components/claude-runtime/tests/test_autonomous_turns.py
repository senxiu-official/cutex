"""A turn Claude Code starts by itself must not be attributed to the next input.

G5 (claude-r21): while an autonomous turn streams with no adapter turn active,
a new input waits for its Result (or a quiet window, or a bound) before the
query is submitted. See docs/BUSY_DELIVERY.md, "Autonomous turns".
"""
from __future__ import annotations
import asyncio

import cute_claude.sdk_runtime as sdk_runtime
from cute_claude.store import dump
from test_persistent import ph, until  # noqa: F401  (fixture)


def idle_events(ph, kind):
    return [m for m in ph.out if m.get('method') == 'claude/runtime/event'
            and m['params']['kind'] == kind and m['params']['whileIdle']]


async def started(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    return tid, ph.sdk.clients[0], ph.engine.driver.owners[tid]


async def autonomous(ph, client, owner, text, finish=False, hold=True):
    seen = len(idle_events(ph, 'assistant'))
    await client.write({'type': 'autonomous', 'text': text, 'finish': finish, 'hold': hold})
    await until(lambda: len(idle_events(ph, 'assistant')) > seen)
    if not finish:
        assert owner.autonomous_until is not None


async def test_input_waits_for_autonomous_result_and_owns_only_its_reply(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    await autonomous(ph, client, owner, 'BACKGROUND-START')
    turn = await ph.turn(tid, 'FOREGROUND')
    await until(lambda: len(owner.deferred) == 1)
    await asyncio.sleep(0.1)
    assert client.query_count == 0 and owner.active is None
    # The rest of the autonomous turn and its Result arrive while the input waits.
    await client.write({'type': 'autonomous', 'text': 'BACKGROUND-END', 'finish': True})
    await ph.drain()
    assert client.query_count == 1 and owner.deferred == [] and owner.autonomous_until is None
    assert ph.store.turn(turn)['status'] == 'completed'
    items = dump(ph.store.items(turn))
    assert '[FIXTURE] FOREGROUND' in items and 'BACKGROUND' not in items
    assert len(idle_events(ph, 'result')) == 1


async def test_quiet_window_releases_input_without_autonomous_result(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 0.3)
    tid, client, owner = await started(ph)
    await autonomous(ph, client, owner, 'BACKGROUND', hold=False)
    loop = asyncio.get_running_loop()
    begin = loop.time()
    turn = await ph.turn(tid, 'FOREGROUND')
    await asyncio.sleep(0.05)
    assert client.query_count == 0
    await ph.drain()
    assert loop.time() - begin >= 0.2
    assert ph.store.turn(turn)['status'] == 'completed'
    items = dump(ph.store.items(turn))
    assert '[FIXTURE] FOREGROUND' in items and 'BACKGROUND' not in items


async def test_task_notification_opens_the_start_window(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_START_SECONDS', 0.3)
    tid, client, owner = await started(ph)
    await client.write({'type': 'idle_event'})
    await until(lambda: idle_events(ph, 'TaskNotificationMessage'))
    assert owner.autonomous_until is not None
    turn = await ph.turn(tid, 'FOREGROUND')
    await asyncio.sleep(0.05)
    assert client.query_count == 0 and len(owner.deferred) == 1
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed' and client.query_count == 1


async def test_wait_is_bounded(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_WAIT_SECONDS', 0.3)
    tid, client, owner = await started(ph)
    await autonomous(ph, client, owner, 'BACKGROUND', hold=False)
    turn = await ph.turn(tid, 'FOREGROUND')
    await asyncio.sleep(0.05)
    assert client.query_count == 0
    await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed' and client.query_count == 1


async def test_no_wait_when_idle(ph, monkeypatch):
    # The connect-time init frame and rate limits do not open a window.
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_START_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    await asyncio.sleep(0.05)
    assert owner.autonomous_until is None
    turn = await ph.turn(tid, 'FOREGROUND')
    async with asyncio.timeout(2):
        await ph.drain()
    assert ph.store.turn(turn)['status'] == 'completed' and owner.deferred == []
    # An autonomous turn that already ended leaves nothing to wait for.
    await autonomous(ph, client, owner, 'BACKGROUND', finish=True)
    await until(lambda: idle_events(ph, 'result'))
    assert owner.autonomous_until is None
    second = await ph.turn(tid, 'AGAIN')
    async with asyncio.timeout(2):
        await ph.drain()
    assert ph.store.turn(second)['status'] == 'completed'
    assert 'BACKGROUND' not in dump(ph.store.items(second))


async def test_interrupt_and_close_do_not_wait_for_the_autonomous_turn(ph, monkeypatch):
    monkeypatch.setattr(sdk_runtime, 'AUTONOMOUS_QUIET_SECONDS', 30.0)
    tid, client, owner = await started(ph)
    await autonomous(ph, client, owner, 'BACKGROUND')
    turn = await ph.turn(tid, 'FOREGROUND')
    await until(lambda: len(owner.deferred) == 1)
    async with asyncio.timeout(2):
        await ph.call('turn/interrupt', {'threadId': tid, 'turnId': turn})
        await ph.drain()
    assert ph.store.turn(turn)['status'] == 'interrupted'
    assert client.query_count == 0 and owner.deferred == []
    # Closing while an input waits fails it as never submitted, without waiting.
    held = asyncio.ensure_future(owner.submit('held-turn', 'HELD', lambda message: asyncio.sleep(0)))
    await until(lambda: len(owner.deferred) == 1)
    async with asyncio.timeout(2):
        await owner.close()
    error = await asyncio.wait_for(asyncio.gather(held, return_exceptions=True), 2)
    assert type(error[0]).__name__ == 'TurnInterrupted' and client.query_count == 0
