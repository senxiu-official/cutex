"""Offline cross-process integration with an explicitly supplied Cutex debug build.

No official Claude login, shared Cutex home, or real model requests are used.
"""
import asyncio
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys

import httpx
import pytest

from cute_claude.config import Settings
from cute_claude.client import Client
from cute_claude.transport import websocket, rpc_token
from fake_sdk import install_fake_sdk
from test_subscription import SubHarness


def port(start):
    for number in range(start, start + 40):
        with socket.socket() as sock:
            try:
                sock.bind(('127.0.0.1', number))
                return number
            except OSError:
                pass
    raise RuntimeError('no free fixture port')


def sqlite_sessions(config_dir):
    """Read r217+'s authoritative store; cutex-sessions.json is a sentinel."""
    sentinel = json.loads((config_dir / 'cutex-sessions.json').read_text())
    assert sentinel['cutexSessionStore'] == 'migrated-to-sqlite'
    connection = sqlite3.connect((config_dir / 'cutex-sessions.sqlite3').as_uri() + '?mode=ro', uri=True)
    try:
        return {key: json.loads(raw) for key, raw in connection.execute('SELECT key,record_json FROM sessions')}
    finally:
        connection.close()


def make_local_only_fixture(config_dir, identity):
    """Private offline fixture mutation, before any Cutex service/cache starts.

    The CLI creates named agents only. Preserve the store's revision and indexed
    columns while creating the ordinary-session variant of this same lifecycle.
    Never mutate the legacy JSON sentinel or use this helper on an online store.
    """
    assert (config_dir.parent / '.cutex-test-private-home').is_file()
    original = sqlite_sessions(config_dir)[identity]
    connection = sqlite3.connect((config_dir / 'cutex-sessions.sqlite3').as_uri() + '?mode=rw', uri=True)
    try:
        with connection:
            connection.execute('BEGIN IMMEDIATE')
            revision = int(connection.execute("SELECT value FROM meta WHERE key='store_revision'").fetchone()[0]) + 1
            record = dict(original, agent_enabled=False, formal_agent_name=None,
                          thread_name='claude-offline-test', registration_class='local_only')
            changed = connection.execute('UPDATE sessions SET record_json=?,formal_agent_name=NULL,rev=? WHERE key=?',
                                          (json.dumps(record), revision, identity))
            assert changed.rowcount == 1
            connection.execute("UPDATE meta SET value=? WHERE key='store_revision'", (str(revision),))
        assert connection.execute('SELECT formal_agent_name,rev FROM sessions WHERE key=?', (identity,)).fetchone() == (None, revision)
    finally:
        connection.close()
    assert sqlite_sessions(config_dir)[identity]['agent_enabled'] is False


async def finish_process(process, data=None):
    try:
        return await asyncio.wait_for(process.communicate(data), 40)
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()


@pytest.mark.parametrize("session_only", [False, True])
@pytest.mark.skipif(not os.getenv('CUTEX_INTEGRATION_BINARY'), reason='requires Cutex debug build')
async def test_management_lifecycle_and_event_forwarding(tmp_path, monkeypatch, session_only):
    binary = Path(os.environ['CUTEX_INTEGRATION_BINARY']).resolve(strict=True)
    mcp_binary = binary.with_name('cutex-mcp' + binary.suffix)
    assert mcp_binary.is_file(), 'Build both Cutex debug binaries in the same isolated target directory'
    (tmp_path / '.cutex-test-private-home').touch()
    config_dir = tmp_path / '.cutex'
    config_dir.mkdir()
    bus_port, management_port = port(24850), port(24900)
    assert bus_port != management_port
    token = 'offline-management-fixture-token'
    (config_dir / 'config.json').write_text(json.dumps({
        'agent_bus_enabled': True, 'agent_bus_port': bus_port,
        'agent_bus_token': 'offline-bus-fixture-token', 'management_api_token': token,
        'new_session_notification': 'important',
    }))
    launch_keys = ('PATH', 'PATHEXT', 'SystemRoot', 'WINDIR', 'COMSPEC', 'TEMP', 'TMP',
                   'APPDATA', 'LOCALAPPDATA', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH')
    env = {key: os.environ[key] for key in launch_keys if key in os.environ}
    env['HOME'] = str(tmp_path)
    env['CUTEX_TEST_PRIVATE_HOME'] = str(tmp_path)
    env['CUTEX_TEST_FORBID_REAL_HOME'] = '1'
    env['CUTEX_MANAGEMENT_URL'] = f'http://127.0.0.1:{management_port}'
    env['CUTEX_AGENT_BUS_URL'] = f'http://127.0.0.1:{bus_port}/'
    # The in-process adapter's MCP facade also inherits only this private home.
    monkeypatch.setattr(os, 'environ', env)
    child_options = {'creationflags': subprocess.CREATE_NO_WINDOW} if os.name == 'nt' else {}
    work = tmp_path / 'work'
    work.mkdir()
    account = tmp_path / 'account'
    account.mkdir()
    deployment = tmp_path / 'deployment.json'
    deployment.write_text(json.dumps({
        'python': sys.executable, 'adapter_source': str(Path(__file__).resolve().parents[1]),
        'claude_cli': sys.executable, 'claude_home': str(account), 'tui_binary': sys.executable,
    }))

    async def cli(*args):
        process = await asyncio.create_subprocess_exec(str(binary), *args, env=env, **child_options,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        out, err = await finish_process(process)
        assert process.returncode == 0, err.decode()
        return json.loads(out) if out.strip() else None

    await cli('human', 'claude', 'install', str(deployment))
    record = await cli('human', 'claude', 'new', 'claude-offline-test', '--cwd', str(work), '--model', 'sonnet')
    identity = record['cutex_session_id']
    # Mirror ensure_server's --workspace argument, including the canonical
    # Windows path spelling saved by the native session store.
    work = Path(record['cwd'])
    if session_only:
        make_local_only_fixture(config_dir, identity)
    state = Path(record['claude_session']['state_dir'])
    thread = record['claude_session']['thread_id']
    driver, sdk, logins = install_fake_sdk(monkeypatch, tmp_path)
    # A parent response names its actual model; initialization aliases alone
    # cannot supply that evidence. Only this fixture's response is augmented.
    receive_messages = sdk.ClaudeSDKClient.receive_messages
    async def observed_responses(self):
        async for message in receive_messages(self):
            if message.get('type') == 'assistant' and not message.get('parent_tool_use_id'):
                message = dict(message, model='claude-sonnet-5-5')
            yield message
    monkeypatch.setattr(sdk.ClaudeSDKClient, 'receive_messages', observed_responses)
    h = SubHarness(Settings(work, state, model='sonnet', mock=False, backend='subscription',
        external_owner_id=identity, external_runtime_generation=record['runtime_generation'] + 1), driver)
    await h.init()
    mcp_env = dict(env, CUTEX_AGENT_ID='claude.fixture', CUTEX_RUNTIME_GENERATION='1',
        CUTEX_AGENT_BUS_URL=f'http://127.0.0.1:{bus_port}/', CUTEX_AGENT_BUS_TOKEN='offline-bus-fixture-token')
    mcp = await asyncio.create_subprocess_exec(str(mcp_binary), env=mcp_env, **child_options,
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    out, err = await finish_process(mcp, b'{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n')
    assert mcp.returncode == 0, err.decode()
    descriptor = state/'cutex-mcp-tools.json'
    descriptor.write_text(json.dumps({'executable':str(mcp_binary),'tools':json.loads(out)['result']['tools']}))
    from cute_claude.cutex_tools import load
    from cute_claude.tools import ToolContext
    load(h.tools, descriptor)

    auth = rpc_token(state, 'CUTEX_UNUSED_FIXTURE_TOKEN')
    stop = asyncio.Event()
    transport = asyncio.create_task(websocket(h.server, 'ws://127.0.0.1:0', auth, stop, ready_file=state/'server.json'))
    processes = []
    client = None
    bus_log = manager_log = None
    try:
        async with asyncio.timeout(5):
            while not (state/'server.json').exists():
                await asyncio.sleep(.01)
        bus_log = open(tmp_path/'bus.log', 'wb')
        manager_log = open(tmp_path/'management.log', 'wb')
        async with httpx.AsyncClient(trust_env=False, timeout=30) as http:
            async def healthy(url, process):
                async with asyncio.timeout(10):
                    while True:
                        assert process.returncode is None, (tmp_path/'management.log').read_text() if (tmp_path/'management.log').exists() else 'service exited'
                        try:
                            response = await http.get(url + '/')
                            if response.is_success:
                                return
                        except httpx.TransportError:
                            pass
                        await asyncio.sleep(.05)

            bus = await asyncio.create_subprocess_exec(str(binary), 'agent', 'serve', '--port', str(bus_port), env=env, stdout=bus_log, stderr=bus_log, **child_options)
            processes.append(bus)
            await healthy(f'http://127.0.0.1:{bus_port}', bus)
            manager = await asyncio.create_subprocess_exec(str(binary), 'management', 'serve', '--port', str(management_port), env=env, stdout=manager_log, stderr=manager_log, **child_options)
            processes.append(manager)
            await healthy(env['CUTEX_MANAGEMENT_URL'], manager)
            notifications = await asyncio.create_subprocess_exec(str(binary), 'notify', 'run', env=env,
                stdout=manager_log, stderr=manager_log, **child_options)
            processes.append(notifications)
            started = await cli('human', 'start', identity)
            assert started['cutex']['result']['status'] == 'online'
            headers = {'Authorization': 'Bearer ' + token}
            async def native(method, params, request_id):
                response = await http.post(f'{env["CUTEX_MANAGEMENT_URL"]}/v2/sessions/{identity}/app-server/requests',
                    headers=headers, json={'requestId': request_id, 'native': {'message': {
                        'id': request_id, 'method': method, 'params': params}}})
                assert response.status_code == 200, response.text
                message = response.json()['native']['message']
                assert 'error' not in message, message
                return message['result']

            catalog = await native('claude/models/catalog', {'threadId': thread}, 'catalog-before')
            assert any(row['id'] == 'claude-sonnet-5-5' for row in catalog['entries']), catalog
            status = await native('claude/model/status', {'threadId': thread}, 'model-status')
            assert status['requestedModel'] == 'sonnet' and status['lastObservedModel'] is None, status
            # These are metadata reads through real Cutex HTTP -> adapter WS.
            assert sdk.clients[0].query_count == 0
            session = (await http.get(f'{env["CUTEX_MANAGEMENT_URL"]}/v2/sessions/{identity}', headers=headers)).json()
            assert 'native' in session, session
            assert session['native']['threadId'] == thread
            assert session['runtime']['status'] == 'online'
            assert session['runtime']['backend'] == 'claude_subscription'
            stale = await http.post(f'{env["CUTEX_MANAGEMENT_URL"]}/v2/sessions/{identity}/cutex/requests', headers=headers,
                json={'requestId':'stale-generation-test','method':'cutex/runtime/close','params':{'expectedRuntimeGeneration':0}})
            assert stale.status_code == 409, stale.text
            roster = (await http.get(f'http://127.0.0.1:{bus_port}/api/agents', headers={'Authorization':'Bearer offline-bus-fixture-token'})).json()
            agents = roster
            assert len(agents) == 1 and agents[0].get('cutex_session_id') == identity, agents
            first = h.engine.runtime_status(thread)
            assert sdk.clients[0].query_count == 0
            await cli('human', 'start', identity)
            assert h.engine.runtime_status(thread)['connectionId'] == first['connectionId']
            manager.terminate()
            await asyncio.wait_for(manager.wait(), 5)
            manager = await asyncio.create_subprocess_exec(str(binary), 'management', 'serve', '--port', str(management_port), env=env, stdout=manager_log, stderr=manager_log, **child_options)
            processes.append(manager)
            await healthy(env['CUTEX_MANAGEMENT_URL'], manager)
            async with asyncio.timeout(10):
                while True:
                    session = (await http.get(f'{env["CUTEX_MANAGEMENT_URL"]}/v2/sessions/{identity}', headers=headers)).json()
                    if session['runtime']['status'] == 'online':
                        break
                    await asyncio.sleep(.05)
            assert h.engine.runtime_status(thread)['connectionId'] == first['connectionId']
            assert sdk.clients[0].query_count == 0
            binding = json.loads((state/'server.json').read_text())
            client = await Client.open(binding['endpoint'], auth)
            if session_only:
                turn = (await client.call('turn/start', {'threadId':thread, 'input':[{'type':'text','text':'ordinary session notification probe'}]}))['turn']['id']
                assert (await client.wait_turn(turn))['status'] == 'completed'
                assert sdk.clients[0].query_count == 1
            else:
                turn = (await client.call('turn/start', {'threadId':thread, 'input':[{'type':'text','text':'TOOL:alarm_list'}]}))['turn']['id']
                assert (await client.wait_turn(turn))['status'] == 'completed'
                assert any(i['type'] == 'mcpToolCall' and i['server'] == 'cutex' for i in h.store.items(turn))
                result = await h.tools.execute('alarm_list', {}, ToolContext(thread, turn, 'list-probe', work))
                assert result.success, result.text
                assert json.loads(result.text)['ok'] is True, result.text
                # The MCP `send` tool was removed (2026-10-07 tool rename) and
                # agents cannot tell themselves; drive the same inbound Claude
                # delivery through the bus send route as this runtime.
                runtime_id = agents[0]['id']
                async def self_send():
                    sent = await http.post(f'http://127.0.0.1:{bus_port}/api/messages/send',
                        headers={'Authorization':'Bearer offline-bus-fixture-token'},
                        json={'to':identity,'content':'Claude inbound integration probe','external_message_id':'claude-self-probe',
                              'delivery_mode':'after_turn','kind':'message','from_agent_id':runtime_id,'all_groups':False,'all_hosts':False})
                    assert sent.status_code == 200, sent.text
                    return sent.json()
                response = await self_send()
                assert response.get('requiredAckLevel', response.get('required_ack_level')) == 'local_inbox', response
                mid = response['id']
                async with asyncio.timeout(12):
                    while True:
                        row = h.store.db.execute('SELECT state FROM inputs WHERE message_id=?', (mid,)).fetchone()
                        if row and row[0] == 'completed':
                            break
                        await asyncio.sleep(.05)
                assert sdk.clients[0].query_count == 2
                display = h.store.input_presentation(thread, mid)
                assert 'claude-offline-test' in display['presentation']['title']
                timeline = await client.call('thread/timeline/list', {'threadId':thread, 'limit':100})
                assert any(e['type'] == 'presentation' and e['item'] == display for e in timeline['data'])
                assert (await self_send())['id'] == mid
                assert sdk.clients[0].query_count == 2

            async with asyncio.timeout(5):
                while True:
                    session = (await http.get(f'{env["CUTEX_MANAGEMENT_URL"]}/v2/sessions/{identity}', headers=headers)).json()
                    if session['runtime']['activeTurnId'] is None and session['runtime']['threadStatus'] == {'type':'idle'}:
                        break
                    await asyncio.sleep(.05)
            async with asyncio.timeout(10):
                while True:
                    snapshot = await cli('notify', 'states')
                    matching = [s for s in snapshot['sessions'] if s['threadId'] == thread]
                    if snapshot['healthy'] and matching and matching[0]['state'] == 'waiting':
                        assert matching[0]['unread'] is True
                        assert matching[0]['agentName'] == 'claude-offline-test'
                        break
                    await asyncio.sleep(.1)
            # Reusing a read request ID must refresh the observation, not serve
            # an idempotency-cache response from before the turn.
            status = await native('claude/model/status', {'threadId': thread}, 'model-status')
            assert status['requestedModel'] == 'sonnet', status
            assert status['lastObservedModel'] == 'claude-sonnet-5-5', status
            assert status['appliesToNextTurn'] is False, status
            events = [json.loads(line) for line in (config_dir/'runtime/management-v2/events.jsonl').read_text().splitlines()]
            completed = [e for e in events if e.get('native',{}).get('message',{}).get('method') == 'turn/completed']
            assert completed and completed[-1]['schema']['protocol'] == 'claude-app-server-subset'
            assert completed[-1]['correlation']['threadId'] == thread
            assert len(sqlite_sessions(config_dir)) == 1
            bootstrap = await http.get(f'{env["CUTEX_MANAGEMENT_URL"]}/v2/sessions/{identity}/bootstrap', headers=headers)
            assert bootstrap.status_code == 200, bootstrap.text
            assert bootstrap.json()['native']['result']['thread']['id'] == thread
            await cli('human', 'stop', identity)
            assert h.engine.runtime_status(thread)['processExitConfirmed'] is True
            session = (await http.get(f'{env["CUTEX_MANAGEMENT_URL"]}/v2/sessions/{identity}', headers=headers)).json()
            assert session['runtime']['status'] == 'offline'
            roster = await http.get(f'http://127.0.0.1:{bus_port}/api/agents', headers={'Authorization':'Bearer offline-bus-fixture-token'})
            assert roster.json() == []
            assert len(sdk.clients) == 1
    finally:
        if client:
            await client.close()
        for process in reversed(processes):
            if process.returncode is None:
                process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), 5)
                except TimeoutError:
                    process.kill()
                    await process.wait()
        stop.set()
        await transport
        await h.close()
        for log in (bus_log, manager_log):
            if log is not None:
                log.close()
