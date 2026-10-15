"""Client-visible r8 runtime routes, using the process-backed SDK double."""
import pytest
from dataclasses import replace
from test_persistent import ph


async def test_mcp_runtime_status_and_controls_use_the_connected_owner(ph):
    tid = await ph.thread()
    offline = await ph.call('mcpServerStatus/list', {'threadId': tid})
    assert offline['data'][0]['runtimeStatus'] == 'notStarted'
    assert not ph.sdk.clients  # status must not start a model or native process
    await ph.call('claude/runtime/start', {'threadId': tid})
    result = await ph.call('mcpServerStatus/list', {'threadId': tid, 'detail': 'toolsAndAuthOnly'})
    assert result['data'][0]['runtimeStatus'] == 'connected'
    assert result['data'][0]['name'] == 'cutex'
    assert 'config' not in result['data'][0]
    await ph.call('mcpServer/toggle', {'threadId': tid, 'serverName': 'cutex', 'enabled': False})
    await ph.call('mcpServer/reconnect', {'threadId': tid, 'serverName': 'cutex'})
    assert ph.sdk.clients[0].controls == [('mcp_status',), ('mcp_toggle', 'cutex', False), ('mcp_reconnect', 'cutex')]
    error = await ph.call('mcpServer/reconnect', {'threadId': tid, 'serverName': 'unmanaged'}, error=True)
    assert error['code'] == -32602


async def test_observed_shell_task_can_be_listed_and_stopped_without_stopping_agents(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    owner = ph.engine.driver.owners[tid]
    await owner._event({'type': 'system', 'subtype': 'task_started', 'session_id': owner.session_id,
                        'task_id': 'shell-1', 'task_type': 'local_bash', 'description': 'build'})
    await owner._event({'type': 'system', 'subtype': 'task_started', 'session_id': owner.session_id,
                        'task_id': 'child-1', 'task_type': 'local_agent', 'description': 'research'})
    result = await ph.call('thread/backgroundTerminals/list', {'threadId': tid})
    assert [row['processId'] for row in result['data']] == ['shell-1']
    assert await ph.call('thread/backgroundTerminals/terminate', {'threadId': tid, 'processId': 'child-1'}) == {'terminated': False}
    assert await ph.call('thread/backgroundTerminals/terminate', {'threadId': tid, 'processId': 'shell-1'}) == {'terminated': True}
    assert ('task_stop', 'shell-1') in ph.sdk.clients[0].controls
    assert ('task_stop', 'child-1') not in ph.sdk.clients[0].controls


async def test_metadata_name_search_and_host_routing_errors(ph):
    tid = await ph.thread()
    await ph.call('thread/name', {'threadId': tid, 'name': 'Managed project'})
    result = await ph.call('thread/metadata/update', {'threadId': tid, 'gitInfo': {'branch': 'r8'}})
    assert result['thread']['name'] == 'Managed project'
    assert result['thread']['gitInfo']['branch'] == 'r8'
    for method in ['thread/shellCommand', 'fs/readFile', 'process/spawn', 'fuzzyFileSearch']:
        error = await ph.call(method, {'threadId': tid}, error=True)
        assert error['code'] == -32601 and '/v2/sessions/' in error['data']['hint']
    assert not ph.sdk.clients


async def test_mutating_history_refuses_racing_start_or_settings(ph):
    tid = await ph.thread()
    with ph.store.history_mutation(tid):
        for method in ('claude/runtime/start', 'thread/settings/update'):
            error = await ph.call(method, {'threadId': tid}, error=True)
            assert error['code'] == -32009
    assert not ph.sdk.clients


async def test_non_sdk_structural_routes_have_matching_typed_feature_errors(ph):
    ph.server.settings = replace(ph.settings, backend='mock')
    for method, feature in [('thread/revert', 'historyRevert'), ('thread/fork', 'fork'), ('thread/delete', 'threadDelete')]:
        assert ph.server.features()[feature] is False
        error = await ph.call(method, {'threadId': 'unused'}, error=True)
        assert error['code'] == -32601
        assert error['data'] == {'unsupported': True, 'runtime': 'claude', 'feature': feature, 'method': method}
    assert not ph.sdk.clients
