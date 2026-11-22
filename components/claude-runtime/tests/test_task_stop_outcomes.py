"""Stop receipts distinguish SDK acknowledgement from task termination."""
import asyncio

import pytest

from test_persistent import ph


async def running(ph, tid, task_id='shell-1'):
    await ph.call('claude/runtime/start', {'threadId': tid})
    owner = ph.engine.driver.owners[tid]
    await owner._event({'type': 'system', 'subtype': 'task_started',
                        'session_id': owner.session_id, 'task_id': task_id,
                        'task_type': 'local_bash', 'description': 'build'})
    return owner


async def stop(ph, tid, task_id='shell-1'):
    return await ph.call('claude/tasks/stop', {'threadId': tid, 'taskId': task_id})


async def test_ack_is_bound_and_does_not_end_task(ph):
    tid = await ph.thread()
    owner = await running(ph, tid)
    result = await stop(ph, tid)
    assert result == {'threadId': tid, 'taskId': 'shell-1', 'outcome': 'accepted',
                      'reason': 'sdkAcknowledged', 'stopRequested': True, 'terminalConfirmed': False}
    assert owner.tasks.rows['shell-1']['status'] == 'running'
    assert ph.sdk.clients[0].controls == [('task_stop', 'shell-1')]


async def test_offline_refuses_without_starting_runtime(ph):
    tid = await ph.thread()
    result = await stop(ph, tid)
    assert result['outcome'] == 'refused' and result['reason'] == 'runtimeOffline'
    assert result['stopRequested'] is False and result['terminalConfirmed'] is False
    assert ph.sdk.clients == []


@pytest.mark.parametrize('status', [None, 'completed', 'stopped', 'unknown'])
async def test_non_running_task_refused_before_sdk_write(ph, status):
    tid = await ph.thread()
    owner = await running(ph, tid)
    if status is None:
        owner.tasks.rows.clear()
    else:
        owner.tasks.rows['shell-1']['status'] = status
    result = await stop(ph, tid)
    assert result['outcome'] == 'refused' and result['reason'] == 'taskNotRunning'
    assert result['stopRequested'] is False and result['terminalConfirmed'] is False
    assert ph.sdk.clients[0].controls == []


@pytest.mark.parametrize('failure', [ConnectionError, RuntimeError, TimeoutError])
async def test_exception_after_sdk_write_is_unknown_without_retry(ph, monkeypatch, failure):
    tid = await ph.thread()
    owner = await running(ph, tid)
    client = ph.sdk.clients[0]
    writes = []

    async def lose_reply(task_id):
        assert asyncio.current_task() is client.owner_task
        writes.append(task_id)
        raise failure('private native failure text')

    monkeypatch.setattr(client, 'stop_task', lose_reply)
    result = await stop(ph, tid)
    assert result['outcome'] == 'unknown' and result['stopRequested'] is None
    assert result['terminalConfirmed'] is False
    assert 'private native' not in str(result)
    assert writes == ['shell-1']
    assert owner.tasks.rows['shell-1']['status'] == 'running'


async def test_stop_cannot_reach_task_in_another_thread(ph):
    first, second = await ph.thread(), await ph.thread()
    await running(ph, first)
    await ph.call('claude/runtime/start', {'threadId': second})
    result = await stop(ph, second)
    assert result['threadId'] == second and result['taskId'] == 'shell-1'
    assert result['outcome'] == 'refused'
    assert all(client.controls == [] for client in ph.sdk.clients)


async def test_owner_disappears_during_control_is_unknown(ph, monkeypatch):
    tid = await ph.thread()
    await running(ph, tid)

    async def disappear(*args, **kwargs):
        raise ConnectionError('reply lost')

    monkeypatch.setattr(ph.engine, 'runtime_control', disappear)
    result = await stop(ph, tid)
    assert result['outcome'] == 'unknown' and result['stopRequested'] is None
    assert result['terminalConfirmed'] is False


async def test_invalid_task_id_is_rejected_before_native_controls(ph):
    tid = await ph.thread()
    await running(ph, tid)
    result = await ph.call('claude/tasks/stop', {'threadId': tid, 'taskId': ''}, error=True)
    assert result['code'] == -32602
    assert ph.sdk.clients[0].controls == []


async def test_terminal_alias_does_not_claim_success_on_lost_stop_reply(ph, monkeypatch):
    tid = await ph.thread()
    await running(ph, tid)

    async def lose_reply(task_id):
        raise ConnectionError('lost stop reply')

    monkeypatch.setattr(ph.sdk.clients[0], 'stop_task', lose_reply)
    result = await ph.call('thread/backgroundTerminals/terminate',
                           {'threadId': tid, 'processId': 'shell-1'}, error=True)
    assert result['data']['outcome'] == 'unknown'
    assert result['data']['terminalConfirmed'] is False


async def test_replacement_owner_cannot_receive_old_stop(ph, monkeypatch):
    tid = await ph.thread()
    await running(ph, tid)
    control = ph.engine.runtime_control

    async def replaced(thread_id, kind, **arguments):
        # A queued stop admitted against a previous connection must not reach
        # even an identically named running task on the replacement connection.
        arguments['expected_connection_id'] = 'previous-connection'
        return await control(thread_id, kind, **arguments)

    monkeypatch.setattr(ph.engine, 'runtime_control', replaced)
    result = await stop(ph, tid)
    assert result['outcome'] == 'refused'
    assert ph.sdk.clients[0].controls == []


async def test_control_deadline_is_unknown_without_retry(ph, monkeypatch):
    tid = await ph.thread()
    await running(ph, tid)
    ph.engine.driver.control_timeout = 0.01
    calls = []

    async def no_reply(task_id):
        calls.append(task_id)
        await asyncio.Event().wait()

    monkeypatch.setattr(ph.sdk.clients[0], 'stop_task', no_reply)
    result = await stop(ph, tid)
    assert result['outcome'] == 'unknown' and result['stopRequested'] is None
    assert calls == ['shell-1'] and result['terminalConfirmed'] is False
