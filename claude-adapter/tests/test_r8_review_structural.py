"""Host routing and non-atomic file/history failures from the r8 review."""
import json
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import pytest

from cute_claude.errors import RpcError
from cute_claude.history import History, thread_wire
from cute_claude.r8_protocol import R8Protocol
from cute_claude.r8_services import read_usage
from test_r8_sessions import native
from test_persistent import ph


def managed_rpc(native, client='codex-tui'):
    peer = SimpleNamespace(subscriptions={native.tid}, clientInfo={'name': client})
    server = SimpleNamespace(engine=native.engine, store=native.store,
        settings=SimpleNamespace(backend='subscription', external_owner_id='owner'), peers=[peer],
        _thread_wire=lambda tid: thread_wire(native.store, History(native.store), tid))
    rpc = R8Protocol(server)
    rpc.sessions = native.ops
    return rpc, peer


@pytest.mark.parametrize('interactive', [False, True])
async def test_initialized_tui_and_noninteractive_rpc_peers_cannot_bypass_managed_routing(ph, interactive):
    tid = await ph.thread()
    ph.server.settings = replace(ph.settings, external_owner_id='cutex-owner', external_runtime_generation=1)
    ph.peer.interactive = interactive
    for method, feature in [('thread/fork', 'fork'), ('thread/revert', 'historyRevert'), ('thread/delete', 'threadDelete')]:
        error = await ph.call(method, {'threadId': tid}, error=True)
        assert error['code'] == -32601 and error['data']['feature'] == feature
        assert error['data']['reason'] == 'cutexManagedSession'
    assert not ph.sdk.clients
    assert ph.store.thread(tid)


@pytest.mark.parametrize('client', ['codex-tui', 'cutex', 'invented-controller'])
@pytest.mark.parametrize(('method', 'feature'), [('thread/fork', 'fork'),
    ('thread/revert', 'historyRevert'), ('thread/delete', 'threadDelete'), ('claude/thread/fork', 'fork')])
async def test_managed_direct_history_is_rejected_independently_of_claimed_client(native, client, method, feature):
    n = native
    rpc, peer = managed_rpc(n, client)
    before = n.path.read_bytes()
    with pytest.raises(RpcError) as error:
        await rpc.dispatch(peer, method, {'threadId': n.tid, 'beforeTurnId': n.turns[1]})
    assert error.value.code == -32601
    assert error.value.data['feature'] == feature
    assert error.value.data['unsupported'] is True
    assert error.value.data['reason'] == 'cutexManagedSession'
    assert 'Cutex' in error.value.data['hint']
    assert n.path.read_bytes() == before
    assert n.engine.session(n.tid)['session_id'] == n.sid
    assert n.store.db.execute('SELECT count(*) FROM session_operations').fetchone()[0] == 0


async def test_managed_alias_exports_a_separate_state_and_reverts_with_receipt(native, tmp_path):
    n = native
    rpc, peer = managed_rpc(n)
    target = tmp_path / 'managed-child'
    fork, after = await rpc.dispatch(peer, 'claude/thread/fork', {'threadId': n.tid, 'targetStateDir': str(target)})
    assert fork['stateDir'] == str(target) and after is None
    assert n.store.db.execute('SELECT count(*) FROM threads').fetchone()[0] == 1
    result, after = await rpc.dispatch(peer, 'claude/thread/revert', {'threadId': n.tid, 'beforeTurnId': n.turns[1]})
    assert result['historyCommitted'] is True
    assert result['nativeSessionId'] != n.sid
    assert result['removedTurnIds'] == [n.turns[1]]
    assert Path(result['receiptPath']).is_file() and after is not None


@pytest.mark.parametrize('failure_phase', ['close', 'fork', 'projection'])
async def test_acknowledged_file_rewind_failure_has_typed_partial_receipt(native, tmp_path, monkeypatch, failure_phase):
    n = native
    file = tmp_path / 'edited.txt'
    file.write_text('current contents', encoding='utf-8')
    async def rewind(tid, kind, **params):
        assert kind == 'rewind_files'
        assert params['user_message_id']
        file.write_text('checkpoint contents', encoding='utf-8')
    n.engine.runtime_control = rewind
    async def fail(*args, **kwargs):
        raise RuntimeError('sensitive provider detail')
    if failure_phase == 'close':
        monkeypatch.setattr(n.ops, 'close', fail)
    elif failure_phase == 'fork':
        monkeypatch.setattr(n.api, 'call', fail)
    else:
        n.store.db.execute("CREATE TRIGGER reject_binding BEFORE UPDATE ON sdk_sessions BEGIN SELECT RAISE(ABORT,'test projection failure'); END")
    with pytest.raises(RpcError) as error:
        await n.ops.revert(n.tid, n.turns[1], rewind_files=True)
    data = error.value.data
    assert error.value.code == -32009 and data['reason'] == 'fileRewindPartialFailure'
    assert data['filesRewound'] is True and data['historyCommitted'] is False
    assert data['outcomeUnknown'] is True
    assert file.read_text(encoding='utf-8') == 'checkpoint contents'
    assert n.engine.session(n.tid)['session_id'] == n.sid
    assert [turn['id'] for turn in History(n.store).full_read(n.tid)] == n.turns
    assert 'sensitive provider detail' not in json.dumps(data)
    receipt = json.loads(Path(data['receiptPath']).read_text(encoding='utf-8'))
    assert receipt == data['receipt']
    assert receipt['state'] == 'partial' and receipt['warnings'][0]['code'] == data['reason']
    stored = n.store.db.execute('SELECT state,data FROM session_operations WHERE id=?', (data['actionId'],)).fetchone()
    assert stored['state'] == 'partial' and json.loads(stored['data']) == receipt


async def test_successful_file_and_history_rewind_reports_both_effects(native):
    async def rewind(*args, **kwargs):
        pass
    native.engine.runtime_control = rewind
    result = await native.ops.revert(native.tid, native.turns[1], rewind_files=True)
    assert result['filesRewound'] is True and result['historyCommitted'] is True
    assert 'warnings' not in result


async def test_fork_inherited_usage_does_not_add_model_requests(native):
    n = native
    n.store.db.execute('UPDATE turns SET usage=? WHERE id=?',
        (json.dumps({'modelRequests': [{'usage': {'input_tokens': 9, 'output_tokens': 4}}]}), n.turns[0]))
    before = read_usage(n.store, {})
    result = await n.ops.fork(n.tid)
    assert read_usage(n.store, {}) == before
    assert read_usage(n.store, {'threadId': result['threadId']})['summary']['lifetimeTokens'] is None
