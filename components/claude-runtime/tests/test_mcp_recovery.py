"""Real pinned SDK + MCP memory transport, no Claude CLI or daemon connection."""
import asyncio
import json
from types import SimpleNamespace

import pytest

sdk = pytest.importorskip('claude_agent_sdk')
from claude_agent_sdk._internal.sdk_mcp_bridge import SdkMcpBridge
from cute_claude.mcp_recovery import RecoveringMcpBridge, RecoveringSdkClient
from cute_claude.subscription import CALL_FIELD, SdkToolBridge


def make_bridge(handler):
    server = sdk.create_sdk_mcp_server(name='cutex', tools=[sdk.SdkMcpTool(
        name='probe', description='offline transport probe',
        input_schema={'type': 'object', 'properties': {'mode': {'type': 'string'}}}, handler=handler)])
    return SdkMcpBridge('cutex', server['instance'])


async def initialize(bridge):
    reply = await bridge.handle({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
        'protocolVersion': '2025-11-25', 'capabilities': {}, 'clientInfo': {'name': 'test', 'version': '1'}}})
    assert 'result' in reply
    await bridge.handle({'jsonrpc': '2.0', 'method': 'notifications/initialized'})


async def call(bridge, request_id, mode):
    async with asyncio.timeout(4):
        return await bridge.handle({'jsonrpc': '2.0', 'id': request_id, 'method': 'tools/call',
                                    'params': {'name': 'probe', 'arguments': {'mode': mode}}})


def host_bridge():
    bridge = SdkToolBridge.__new__(SdkToolBridge)
    bridge.binding = ('turn', 'nonce')
    bridge.accepting = True
    bridge.grants = {}
    bridge.host_tasks = set()
    return bridge


async def test_host_child_cancel_is_an_error_result_and_parallel_sdk_tools_survive():
    host = host_bridge()
    running = {}
    started = asyncio.Event()
    executions = []

    async def execute_bound(turn, name, request):
        mode = request['input']['mode']
        executions.append(mode)
        if mode == 'cancel':
            running[mode] = asyncio.current_task()
            started.set()
            await asyncio.Event().wait()
        await asyncio.sleep(.05)
        return {'content': [{'type': 'text', 'text': 'ok'}]}

    host._execute_bound = execute_bound

    async def handler(args):
        mode = args['mode']
        host.grants[mode] = (host.binding, {'name': 'probe', 'input': args})
        return await host.execute('probe', {'arguments': args, CALL_FIELD: mode})

    raw = make_bridge(handler)
    try:
        await initialize(raw)
        failed = asyncio.create_task(call(raw, 2, 'cancel'))
        sibling = asyncio.create_task(call(raw, 3, 'sibling'))
        await asyncio.wait_for(started.wait(), 2)
        running['cancel'].cancel()
        first, second = await asyncio.gather(failed, sibling)
        assert first['result']['isError']
        assert 'not replayed' in first['result']['content'][0]['text']
        assert not second['result'].get('isError')
        assert not raw._session.finished
        third = await call(raw, 4, 'next')
        assert not third['result'].get('isError')
        assert sorted(executions) == ['cancel', 'next', 'sibling']
        assert not host.host_tasks
    finally:
        await raw.aclose()


async def test_cancellation_of_the_mcp_request_itself_is_not_swallowed():
    host = host_bridge()
    started = asyncio.Event()

    async def execute_bound(*args):
        started.set()
        await asyncio.Event().wait()

    host._execute_bound = execute_bound
    host.grants['one'] = (host.binding, {'name': 'probe', 'input': {}})
    task = asyncio.create_task(host.execute('probe', {'arguments': {}, CALL_FIELD: 'one'}))
    await asyncio.wait_for(started.wait(), 2)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert not host.host_tasks


async def test_unexpected_server_exit_rebuilds_once_for_parallel_calls_without_replay():
    executions = []

    async def handler(args):
        executions.append(args['mode'])
        if args['mode'] == 'cancel':
            await asyncio.sleep(.03)
            raise asyncio.CancelledError('injected foreign handler cancellation')
        await asyncio.sleep(.1)
        return {'content': [{'type': 'text', 'text': 'ok'}]}

    recovered = RecoveringMcpBridge(make_bridge(handler))
    try:
        await initialize(recovered)
        original = recovered.bridge._session
        results = await asyncio.gather(call(recovered, 2, 'cancel'), call(recovered, 3, 'sibling'))
        for result in results:
            assert result['result']['isError']
            detail = json.loads(result['result']['content'][0]['text'])
            assert detail['outcomeUnknown'] and not detail['retryable']
        assert recovered.recoveries == 1
        assert recovered.bridge._session is not original
        assert not (await call(recovered, 4, 'next'))['result'].get('isError')
        assert sorted(executions) == ['cancel', 'next', 'sibling']
    finally:
        await recovered.aclose()
    with pytest.raises(RuntimeError, match='closed'):
        await call(recovered, 5, 'after-close')
    assert recovered.recoveries == 1


async def test_dead_bridge_installed_after_sdk_connect_can_recover_without_cached_handshake():
    async def handler(args):
        if args['mode'] == 'cancel':
            raise asyncio.CancelledError()
        return {'content': [{'type': 'text', 'text': 'ok'}]}

    raw = make_bridge(handler)
    await initialize(raw)
    await call(raw, 2, 'cancel')
    await asyncio.sleep(.01)
    assert raw._session.finished

    class Client:
        _query = None
        async def connect(self):
            self._query = SimpleNamespace(_sdk_mcp_bridges={'cutex': raw, 'external': object()})

    client = Client()
    wrapped = RecoveringSdkClient(client)
    await wrapped.connect()
    recovered = client._query._sdk_mcp_bridges['cutex']
    try:
        assert isinstance(recovered, RecoveringMcpBridge)
        assert client._query._sdk_mcp_bridges['external'] is not None
        assert not (await call(recovered, 3, 'next'))['result'].get('isError')
    finally:
        await recovered.aclose()


async def test_failed_recovery_is_retryable_without_executing_the_waiting_tool():
    executions = []

    async def handler(args):
        executions.append(args['mode'])
        if args['mode'] == 'cancel':
            raise asyncio.CancelledError()
        return {'content': [{'type': 'text', 'text': 'ok'}]}

    raw = make_bridge(handler)
    recovered = RecoveringMcpBridge(raw)
    try:
        await initialize(recovered)
        await call(raw, 2, 'cancel')
        await asyncio.sleep(.01)
        original_handle = raw.handle
        failures = 0

        async def flaky(message):
            nonlocal failures
            if message.get('method') == 'initialize' and failures == 0:
                failures += 1
                return {'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32603, 'message': 'injected handshake error'}}
            return await original_handle(message)

        raw.handle = flaky
        refused = await call(recovered, 3, 'not-executed')
        detail = json.loads(refused['result']['content'][0]['text'])
        assert detail['retryable'] and not detail['outcomeUnknown']
        assert executions == ['cancel']
        assert not (await call(recovered, 4, 'next'))['result'].get('isError')
        assert executions == ['cancel', 'next']
    finally:
        await recovered.aclose()


async def test_real_sdk_query_routes_only_cutex_through_recovery():
    from claude_agent_sdk._internal.query import Query

    async def handler(args):
        return {'content': [{'type': 'text', 'text': 'ok'}]}

    raw = make_bridge(handler)
    query = Query(transport=object(), is_streaming_mode=True, sdk_mcp_servers={'cutex': raw._server})
    external = object()
    query._sdk_mcp_bridges['external'] = external

    class Client:
        _query = query
        async def connect(self):
            pass

    client = RecoveringSdkClient(Client())
    await client.connect()
    installed = query._sdk_mcp_bridges['cutex']
    try:
        assert isinstance(installed, RecoveringMcpBridge)
        assert query._sdk_mcp_bridges['external'] is external
        await initialize(installed)
        reply = await query._handle_sdk_mcp_request('cutex', {
            'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call',
            'params': {'name': 'probe', 'arguments': {'mode': 'query-path'}},
        })
        assert not reply['result'].get('isError')
    finally:
        await installed.aclose()
