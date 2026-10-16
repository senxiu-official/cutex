from __future__ import annotations
import asyncio
from dataclasses import replace
import json
import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace

import pytest
import pytest_asyncio

from cute_claude.config import Settings
from cute_claude.protocol import Server
from cute_claude.sdk_runtime import NativeSessionLease
from cute_claude.subscription import OfficialSdkDriver, SdkToolBridge, SubscriptionEngine, CALL_FIELD
from cute_claude.subscription_auth import check_official_login, resolve_config_dir, official_cli, CONFLICTING_ENV
from cute_claude.provider import ProviderError
from cute_claude.store import Store, identifier, dump
from cute_claude.tools import ToolRegistry
from fake_sdk import install_fake_sdk, materialize
from test_subscription import SubHarness


@pytest_asyncio.fixture
async def ph(tmp_path, monkeypatch):
    driver, sdk, logins = install_fake_sdk(monkeypatch, tmp_path)
    work = tmp_path/'work'; work.mkdir()
    (work/'hello.txt').write_text('file from persistent fixture')
    h = SubHarness(Settings(work, tmp_path/'state', model='sonnet', mock=False, backend='subscription'), driver)
    h.sdk, h.logins = sdk, logins
    await h.init()
    try:
        yield h
    finally:
        await h.close()


async def until(predicate):
    async with asyncio.timeout(5):
        while not predicate(): await asyncio.sleep(0.005)


async def test_online_is_connection_only_and_two_turns_keep_pid(ph):
    tid = await ph.thread()
    online = await ph.call('claude/runtime/start', {'threadId':tid})
    assert online['state'] == 'waiting' and online['processId'] > 0
    client = ph.sdk.clients[0]
    assert client.options.max_turns is None
    assert client.query_count == 0
    first = await ph.turn(tid, 'one')
    await ph.drain()
    second = await ph.turn(tid, 'two')
    await ph.drain()
    after = await ph.call('claude/runtime/status', {'threadId':tid})
    assert after['processId'] == online['processId']
    assert after['sessionId'] == online['sessionId']
    assert after['connectionId'] == online['connectionId']
    assert client.query_count == 2 and client.disconnect_count == 0 and client.readers == 1
    assert ph.logins == [], 'no pre-start auth status (OAuth refresh-lock race)'
    assert all(ph.store.turn(t)['status'] == 'completed' for t in (first,second))
    assert ph.engine.driver.owners[tid].task is client.owner_task
    assert ph.engine.driver.owners[tid].reader is client.reader_task


async def test_idle_notification_consumed_without_prompt(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    client = ph.sdk.clients[0]
    await client.write({'type':'idle_event'})
    await until(lambda: any(m.get('method') == 'claude/runtime/event' and
        m['params']['kind'] == 'TaskNotificationMessage' for m in ph.out))
    assert client.query_count == 0 and client.readers == 1
    events = ph.store.events(tid, 0, 200)
    assert 'not public log content' not in dump(events)


async def test_same_directory_two_independent_native_sessions(ph):
    a, b = await ph.thread(), await ph.thread()
    ta, tb = await ph.turn(a, 'ALPHA'), await ph.turn(b, 'BRAVO')
    await ph.drain()
    assert len(ph.sdk.clients) == 2
    sa, sb = ph.engine.runtime_status(a), ph.engine.runtime_status(b)
    assert sa['sessionId'] != sb['sessionId'] and sa['processId'] != sb['processId']
    ha, hb = await ph.turn(a, 'HISTORY'), await ph.turn(b, 'HISTORY')
    await ph.drain()
    assert 'BRAVO' not in dump(ph.store.items(ha))
    assert 'ALPHA' not in dump(ph.store.items(hb))
    oa, ob = ph.sdk.clients[0].options, ph.sdk.clients[1].options
    assert oa.cli_path == ob.cli_path and oa.env['CLAUDE_CONFIG_DIR'] == ob.env['CLAUDE_CONFIG_DIR']
    assert ph.logins == []


async def test_tools_in_second_turn_have_second_turn_ids(ph):
    tid = await ph.thread()
    a = await ph.turn(tid, 'TOOL:read_file'); await ph.drain()
    b = await ph.turn(tid, 'TOOL:read_file'); await ph.drain()
    rows = ph.store.db.execute('SELECT turn_id, call_id FROM tool_calls').fetchall()
    assert {r[0] for r in rows} == {a,b} and len({r[1] for r in rows}) == 2
    assert len(ph.sdk.clients) == 1
    assert all(len([i for i in ph.store.items(t) if i['type']=='dynamicToolCall']) == 1 for t in (a,b))


async def test_late_granted_callback_rejected_after_turn_rollover(ph):
    tid = await ph.thread()
    sid = ph.engine.session(tid)['session_id']
    bridge = SdkToolBridge(ph.engine, tid, 'old-turn', sid)
    data = {'session_id':sid,'tool_name':'mcp__cutex__read_file','tool_input':{'arguments':{'path':'hello.txt'}}}
    permit = await bridge.pre_tool(data, 'old-call', {})
    old_args = permit['hookSpecificOutput']['updatedInput']
    await bridge.end_turn(); bridge.begin_turn('new-turn')
    reply = await bridge.execute('read_file', old_args)
    assert reply['is_error']
    assert ph.store.db.execute('SELECT count(*) FROM tool_calls').fetchone()[0] == 0
    bridge.revoke()
    denied = await bridge.pre_tool(data, 'another-call', {})
    assert denied['hookSpecificOutput']['permissionDecision'] == 'deny'


async def test_detach_and_unsubscribe_do_not_stop_runtime(ph):
    tid = await ph.thread()
    turn = await ph.turn(tid, 'HANG')
    await until(lambda: len(ph.sdk.clients) == 1 and ph.sdk.clients[0].query_count == 1)
    client = ph.sdk.clients[0]
    pid = ph.engine.runtime_status(tid)['processId']
    await ph.call('thread/unsubscribe', {'threadId':tid})
    ph.server.disconnect(ph.peer)
    assert client.disconnect_count == 0 and client._transport._process.returncode is None
    # A second frontend attaches; read-only resume cannot create another writer.
    ph.peer = ph.server.connect(ph.send); await ph.init()
    await ph.call('thread/resume', {'threadId':tid})
    assert ph.engine.runtime_status(tid)['processId'] == pid and len(ph.sdk.clients) == 1
    await ph.call('turn/interrupt', {'threadId':tid,'turnId':turn})
    assert ph.store.turn(turn)['status'] == 'interrupted', (ph.store.turn(turn), ph.engine.runtime_status(tid))


async def test_interrupt_drains_result_keeps_client_and_never_replays_tool(ph):
    begun = asyncio.Event()
    effects = []
    async def slow(args, ctx):
        effects.append((ctx.turn_id, ctx.call_id)); begun.set()
        await asyncio.Event().wait()
    ph.tools.register('slow','slow',{'type':'object','properties':{}},slow)
    tid = await ph.thread()
    t = await ph.turn(tid, 'TOOL:slow')
    await asyncio.wait_for(begun.wait(), 5)
    before = ph.engine.runtime_status(tid)
    await ph.call('turn/interrupt', {'threadId':tid,'turnId':t})
    assert ph.store.turn(t)['status'] == 'interrupted'
    assert ph.store.thread(tid)['halted']
    assert ph.store.db.execute('SELECT state FROM tool_calls').fetchone()[0] == 'uncertain'
    assert ph.engine.runtime_status(tid)['state'] == 'waiting'
    # A fresh TUI turn can continue; no extension-only queue/resume is necessary.
    await ph.call('thread/resume', {'threadId':tid})
    assert ph.store.thread(tid)['halted']
    invalid = await ph.call('turn/start', {'threadId':tid,'input':[]}, error=True)
    assert invalid['code'] == -32602 and ph.store.thread(tid)['halted']
    following = await ph.turn(tid, 'next')
    await ph.drain()
    assert ph.store.turn(following)['status'] == 'completed'
    assert ph.engine.runtime_status(tid)['processId'] == before['processId']
    assert len(effects) == 1 and ph.sdk.clients[0].query_count == 2


async def test_interrupt_timeout_closes_instead_of_misattributing_next_result(ph):
    tid = await ph.thread()
    t = await ph.turn(tid, 'HANG')
    await until(lambda: len(ph.sdk.clients) == 1 and ph.sdk.clients[0].query_count == 1)
    ph.sdk.clients[0].drop_interrupt = True
    await ph.call('turn/interrupt', {'threadId':tid,'turnId':t})
    assert ph.engine.runtime_status(tid)['processExitConfirmed'] is True
    assert ph.store.turn(t)['status'] == 'failed'
    assert ph.store.thread(tid)['halted']
    assert ph.store.db.execute('SELECT state FROM inputs WHERE turn_id=?',(t,)).fetchone()[0] == 'uncertain'
    assert ph.sdk.clients[0].query_count == 1
    reply = await ph.call('turn/start', {'threadId':tid,'input':[{'type':'text','text':'new'}]}, error=True)
    assert reply['code'] == -32009 and ph.store.thread(tid)['halted']


async def test_close_observes_exit_and_restart_resumes_exact_id(ph):
    tid = await ph.thread()
    await ph.turn(tid, 'BEFORE_RESTART'); await ph.drain()
    before = ph.engine.runtime_status(tid)
    closed = await ph.call('claude/runtime/close', {'threadId':tid})
    assert closed['state'] == 'closed' and closed['processExitConfirmed'] is True
    assert ph.sdk.clients[0]._transport._process.returncode is not None
    error = await ph.call('turn/start', {'threadId':tid,'input':[{'type':'text','text':'not allowed while closed'}]}, error=True)
    assert error['code'] == -32009
    after = await ph.call('claude/runtime/start', {'threadId':tid})
    assert after['sessionId'] == before['sessionId'] and after['connectionId'] != before['connectionId']
    assert ph.sdk.clients[1].options.resume == before['sessionId']
    assert ph.sdk.clients[1].query_count == 0
    t = await ph.turn(tid, 'HISTORY'); await ph.drain()
    assert 'BEFORE_RESTART' in dump(ph.store.items(t))
    assert ph.logins == []


async def test_subprocess_crash_is_not_automatically_resubmitted(ph):
    tid = await ph.thread()
    t = await ph.turn(tid, 'CRASH'); await ph.drain()
    assert ph.store.turn(t)['status'] == 'failed'
    assert ph.store.thread(tid)['halted']
    assert ph.sdk.clients[0].query_count == 1 and len(ph.sdk.clients) == 2
    assert ph.sdk.clients[0]._transport._process.returncode is not None
    await ph.call('thread/resume', {'threadId':tid})
    await asyncio.sleep(.02)
    assert len(ph.sdk.clients) == 2
    prior = ph.engine.session(tid)['session_id']
    await ph.call('claude/runtime/start', {'threadId':tid})
    assert ph.engine.session(tid)['session_id'] == prior
    assert ph.sdk.clients[1].options.resume == prior and ph.sdk.clients[1].query_count == 0


async def test_idle_subprocess_death_reported_without_keepalive(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    client = ph.sdk.clients[0]
    client._transport._process.kill()
    await until(lambda: ph.engine.runtime_status(tid)['nativeConnectionGeneration'] == 2)
    assert client.query_count == 0
    info = ph.engine.runtime_status(tid)
    assert client._transport._process.returncode is not None and info['lastError']
    assert info['state'] == 'waiting' and ph.sdk.clients[1].query_count == 0


async def test_concurrent_start_only_one_owner_and_one_process(ph):
    tid = await ph.thread()
    a,b = await asyncio.gather(ph.engine.start_runtime(tid), ph.engine.start_runtime(tid))
    assert a['processId'] == b['processId'] and len(ph.sdk.clients) == 1 and ph.logins == []


async def test_native_lease_blocks_duplicate_copied_binding(ph, tmp_path):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    sid = ph.engine.session(tid)['session_id']
    with pytest.raises(ProviderError, match='Another adapter'):
        NativeSessionLease(ph.engine.driver.config_dir, sid)
    await ph.engine.close_runtime(tid)
    lease = NativeSessionLease(ph.engine.driver.config_dir, sid)
    lease.close()


async def test_item_timestamps_live_and_durable(ph):
    tid = await ph.thread()
    await ph.turn(tid,'TOOL:read_file'); await ph.drain()
    for event in ph.store.events(tid,0,200):
        if event['method'] in {'item/started','item/completed'}:
            key = 'startedAtMs' if event['method']=='item/started' else 'completedAtMs'
            assert type(event['params'][key]) is int and event['params'][key] > 0
    for event in ph.out:
        if event.get('method') in {'item/started','item/completed'}:
            key='startedAtMs' if event['method']=='item/started' else 'completedAtMs'
            assert type(event['params'][key]) is int


@pytest.mark.skipif(sys.platform == 'win32', reason='executes a #! script / extensionless PATH entry (POSIX only)')
async def test_official_status_uses_selected_config_dir(tmp_path, monkeypatch):
    for name in CONFLICTING_ENV: monkeypatch.delenv(name,raising=False)
    config = tmp_path/'isolated'; config.mkdir()
    cli = tmp_path/'fixture-cli'
    cli.write_text('#!'+sys.executable+'\nimport json,os,sys\nassert sys.argv[1:]==["auth","status"]\nassert os.environ["CLAUDE_CONFIG_DIR"]=='+repr(str(config))+'\nprint(json.dumps({"loggedIn":True,"authMethod":"claude.ai"}))\n')
    cli.chmod(0o700)
    assert (await check_official_login(cli, config))['loggedIn']


def test_config_dir_precedence(tmp_path, monkeypatch):
    monkeypatch.setenv('CLAUDE_CONFIG_DIR', str(tmp_path/'environment'))
    assert resolve_config_dir() == tmp_path/'environment'
    assert resolve_config_dir(tmp_path/'explicit') == tmp_path/'explicit'


@pytest.mark.skipif(sys.platform == 'win32', reason='executes a #! script / extensionless PATH entry (POSIX only)')
def test_path_cli_selected_not_sdk_bundle(tmp_path, monkeypatch):
    cli = tmp_path/'claude'; cli.write_text('fixture'); cli.chmod(0o700)
    monkeypatch.setenv('PATH', str(tmp_path))
    assert official_cli() == cli
    monkeypatch.setenv('PATH', str(tmp_path/'absent'))
    with pytest.raises(ValueError, match='not select the SDK-bundled'):
        official_cli()


def test_schema_verifier_uses_v1_initialization():
    import runpy
    data=runpy.run_path(str(Path(__file__).parents[1]/'scripts/verify_r91.py'))
    assert data['RPC']['initialize'] == ('v1/InitializeParams','v1/InitializeResponse')

async def test_close_during_query_does_not_claim_clean_interruption(ph):
    tid = await ph.thread()
    turn = await ph.turn(tid, 'HANG')
    await until(lambda: len(ph.sdk.clients) and ph.sdk.clients[0].query_count == 1)
    closed = await ph.call('claude/runtime/close', {'threadId':tid})
    assert closed['processExitConfirmed'] is True
    assert ph.store.turn(turn)['status'] == 'failed'
    assert ph.store.db.execute('SELECT state FROM inputs WHERE turn_id=?',(turn,)).fetchone()[0] == 'uncertain'
    assert ph.store.thread(tid)['halted']
    assert ph.sdk.clients[0].query_count == 1


async def test_close_before_worker_starts_never_spawns_sdk(ph):
    tid = await ph.thread()
    # Create exactly the durable local claim used by turn/start, without
    # giving its deferred worker a scheduling opportunity before Close.
    payload = {'source':{'kind':'human','id':'user'},'eventType':'user.message','text':'never sent','metadata':{}}
    receipt = ph.store.enqueue(tid, 'close-before-start', payload, ph.settings.max_pending)
    ph.engine.wake(tid)
    closed = await ph.engine.close_runtime(tid)
    assert not ph.sdk.clients
    assert closed['state'] == 'offline'
    assert ph.store.db.execute("SELECT count(*) FROM turns WHERE thread_id=? AND state='inProgress'",(tid,)).fetchone()[0] == 0


async def test_reader_failure_with_active_input_marks_uncertain_and_redacts(ph):
    tid = await ph.thread()
    turn = await ph.turn(tid, 'HANG')
    await until(lambda: len(ph.sdk.clients) and ph.sdk.clients[0].query_count == 1)
    from cute_claude.sdk_runtime import End
    await ph.engine.driver.owners[tid].mailbox.put(End(RuntimeError('SECRET-token-or-prompt')))
    await ph.drain()
    info = ph.engine.runtime_status(tid)
    assert 'SECRET' not in dump(info) and 'RuntimeError' in info['lastError']
    assert 'SECRET' not in dump(ph.store.events(tid,0,500))
    assert ph.store.db.execute('SELECT state FROM inputs WHERE turn_id=?',(turn,)).fetchone()[0] == 'uncertain'
    assert info['state'] == 'waiting' and info['nativeConnectionGeneration'] == 2
    assert ph.sdk.clients[0]._transport._process.returncode is not None


async def test_changed_native_identity_closes_no_silent_rebind(ph):
    tid = await ph.thread()
    await ph.engine.start_runtime(tid)
    old = ph.engine.session(tid)['session_id']
    await ph.sdk.clients[0].events.put({'type':'system','data':{'session_id':identifier()}})
    await until(lambda: ph.engine.driver.owners[tid].task.done())
    assert ph.engine.session(tid)['session_id'] == old
    assert 'different' in ph.engine.runtime_status(tid)['lastError']
    assert ph.sdk.clients[0].query_count == 0


async def test_unsubscribe_wire_status_matches_r91(ph):
    tid = await ph.thread()
    assert (await ph.call('thread/unsubscribe',{'threadId':tid})) == {'status':'unsubscribed'}
    assert (await ph.call('thread/unsubscribe',{'threadId':tid})) == {'status':'notSubscribed'}


async def test_adapter_restart_resumes_from_official_store_not_audit(ph, tmp_path):
    tid = await ph.thread()
    await ph.turn(tid,'PRESERVE_THROUGH_ADAPTER_RESTART'); await ph.drain()
    old = ph.engine.session(tid)['session_id']
    settings = ph.settings
    cli, config = ph.engine.driver.cli, ph.engine.driver.config_dir
    await ph.close()
    async def already_closed(): pass
    ph.close = already_closed
    # Harness.close is called in fixture teardown; use a fresh harness there too.
    driver = OfficialSdkDriver(cli,config,interrupt_timeout=2)
    newer = SubHarness(settings,driver)
    try:
        await newer.init()
        await newer.call('thread/resume',{'threadId':tid})
        assert len(ph.sdk.clients) == 1   # read-only frontend attach
        await newer.call('claude/runtime/start',{'threadId':tid})
        assert ph.sdk.clients[-1].options.resume == old
        assert ph.sdk.clients[-1].query_count == 0
        turn = await newer.turn(tid,'HISTORY'); await newer.drain()
        assert 'PRESERVE_THROUGH_ADAPTER_RESTART' in dump(newer.store.items(turn))
        assert newer.engine.session(tid)['session_id'] == old
    finally:
        await newer.close()
    # Avoid a double database close in shared fixture teardown.
    async def already_closed(): pass
    ph.close = already_closed

async def test_native_lease_blocks_orphan_child_after_adapter_lock_is_released(tmp_path):
    child = await asyncio.create_subprocess_exec(sys.executable, '-c', 'import time; time.sleep(30)')
    sid = identifier()
    lease = NativeSessionLease(tmp_path, sid)
    try:
        lease.record_process(child.pid)
        lease.close()  # Simulate the adapter dying while its child survives.
        with pytest.raises(ProviderError, match='still running'):
            NativeSessionLease(tmp_path, sid)
        child.terminate()
        await child.wait()
        recovered = NativeSessionLease(tmp_path, sid)
        recovered.close()
    finally:
        if child.returncode is None:
            child.kill()
            await child.wait()

async def test_model_picker_switch_keeps_native_identity_and_survives_restart(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    original = ph.engine.runtime_status(tid)
    models = await ph.call('model/list')
    assert {'sonnet','opus','haiku'} <= {m['model'] for m in models['data']}
    await ph.call('thread/settings/update', {'threadId':tid,'model':'opus',
        'collaborationMode':{'mode':'default','settings':{'model':'opus','reasoning_effort':None,'developer_instructions':None}}})
    assert ph.sdk.clients[0].options.model == 'opus'
    current = ph.engine.runtime_status(tid)
    assert (current['connectionId'], current['sessionId']) == (original['connectionId'], original['sessionId'])
    assert ph.sdk.clients[0].query_count == 0
    resume = await ph.call('thread/resume', {'threadId':tid,'model':'sonnet'})
    assert resume['model'] == 'opus'
    saved = ph.store.thread(tid)
    assert ph.store.ensure_thread(tid,'sonnet',saved['cwd'],saved['instructions']) is False
    await ph.call('claude/runtime/close', {'threadId':tid})
    await ph.call('claude/runtime/start', {'threadId':tid})
    assert ph.sdk.clients[-1].options.model == 'opus'
    assert ph.engine.runtime_status(tid)['sessionId'] == original['sessionId']

async def test_profile_effort_reaches_sdk_and_resume(ph):
    from dataclasses import replace
    ph.engine.settings = ph.server.settings = replace(ph.settings, effort='high')
    ph.server.model_config.settings = ph.server.settings
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId':tid})
    assert ph.sdk.clients[0].options.effort == 'high'
    resumed = await ph.call('thread/resume', {'threadId':tid, 'reasoningEffort':'high'})
    assert resumed['reasoningEffort'] == resumed['thread']['reasoningEffort'] == 'high'
    models = await ph.call('model/list')
    # A configured effort is not evidence that every reference/custom model
    # supports it. Only advertised levels can be the picker default.
    assert all(m['defaultReasoningEffort'] == ('high' if 'high' in {
        level['reasoningEffort'] for level in m['supportedReasoningEfforts']} else 'none') for m in models['data'])
    await ph.call('thread/settings/update', {'threadId':tid,'model':'opus','effort':'high'})
    assert ph.sdk.clients[0].options.effort == 'high'

async def test_sdk_model_catalog_and_tui_default_write_are_distinct_from_live_model(ph):
    from cute_claude.model_config import ModelConfig
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    before = ph.engine.runtime_status(tid)
    models = await ph.call('model/list')
    assert {'fable', 'future-model'} <= {m['model'] for m in models['data']}
    edits = [{'keyPath': 'model', 'value': 'fable', 'mergeStrategy': 'replace'},
             {'keyPath': 'model_reasoning_effort', 'value': None, 'mergeStrategy': 'replace'}]
    response = await ph.call('config/batchWrite', {'edits': edits, 'reloadUserConfig': True})
    assert response['status'] == 'ok' and Path(response['filePath']).is_file()
    assert ModelConfig(ph.settings).model() == 'fable'
    assert (await ph.call('config/read'))['config']['model'] == 'fable'
    # Saving defaults must neither send a prompt nor switch a running thread.
    assert ph.engine.runtime_status(tid)['model'] == 'sonnet'
    new = await ph.call('thread/start')
    assert new['model'] == 'fable'
    await ph.call('thread/settings/update', {'threadId': tid, 'model': 'fable'})
    after = ph.engine.runtime_status(tid)
    assert after['model'] == 'fable' and after['connectionId'] == before['connectionId']
    assert ph.sdk.clients[0].query_count == 0
    old_bytes = Path(response['filePath']).read_bytes()
    await ph.call('config/batchWrite', {'edits': edits, 'expectedVersion': 'stale'}, error=True)
    await ph.call('config/batchWrite', {'edits': edits + [
        {'keyPath': 'sandbox_mode', 'value': 'read-only', 'mergeStrategy': 'replace'}]}, error=True)
    assert Path(response['filePath']).read_bytes() == old_bytes

async def test_effort_picker_reconnects_idle_child_preserving_owner_history_and_identity(ph):
    tid = await ph.thread()
    await ph.turn(tid, 'KEEP THIS HISTORY')
    await ph.drain()
    original = ph.engine.runtime_status(tid)
    original_client = ph.sdk.clients[-1]
    models = await ph.call('model/list')
    fable = next(m for m in models['data'] if m['model'] == 'fable')
    assert [x['reasoningEffort'] for x in fable['supportedReasoningEfforts']] == ['none','low','medium','high','xhigh','max']
    await ph.call('thread/settings/update', {'threadId':tid,'model':'fable','effort':'high'})
    after = ph.engine.runtime_status(tid)
    assert after['sessionId'] == original['sessionId'] and after['connectionId'] == original['connectionId']
    assert after['processId'] != original['processId'] and after['nativeConnectionGeneration'] == 2
    assert after['effort'] == 'high' and original_client.disconnect_count == 1
    assert ph.sdk.clients[-1].options.resume == original['sessionId']
    assert ph.sdk.clients[-1].options.effort == 'high'
    assert (await ph.call('thread/resume', {'threadId':tid}))['reasoningEffort'] == 'high'
    second = await ph.turn(tid, 'HISTORY')
    await ph.drain()
    assert 'KEEP THIS HISTORY' in dump(ph.store.items(second))
    # Reapplying settings is a no-op; a model-only change keeps the SDK child.
    await ph.call('thread/settings/update', {'threadId':tid,'model':'sonnet','effort':'high'})
    assert ph.engine.runtime_status(tid)['processId'] == after['processId']
    await ph.call('claude/runtime/close', {'threadId':tid})
    await ph.call('claude/runtime/start', {'threadId':tid})
    assert ph.sdk.clients[-1].options.effort == 'high'
    assert ph.engine.runtime_status(tid)['sessionId'] == original['sessionId']

async def test_effort_reconnect_failure_is_reported_and_does_not_start_a_second_writer(ph, monkeypatch):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    owner = ph.engine.driver.owners[tid]
    original = owner.snapshot()
    make_client = ph.engine.driver.make_client
    async def fail(*args, **kwargs):
        raise ProviderError('fixture reconnect failed')
    monkeypatch.setattr(ph.engine.driver, 'make_client', fail)
    result = await ph.call('thread/settings/update', {'threadId':tid, 'model':'fable','effort':'high'}, error=True)
    assert 'fixture reconnect failed' in result['message']
    await owner.task
    assert owner.snapshot()['processExitConfirmed'] is True
    assert len(ph.sdk.clients) == 1 and ph.sdk.clients[0].disconnect_count >= 1
    monkeypatch.setattr(ph.engine.driver, 'make_client', make_client)
    await ph.call('claude/runtime/start', {'threadId':tid})
    assert ph.engine.runtime_status(tid)['sessionId'] == original['sessionId']
    assert ph.sdk.clients[-1].options.effort == 'high'

async def test_clear_saved_effort_uses_claude_default_even_with_profile_high(ph):
    ph.server.model_config.settings = replace(ph.settings, effort='high')
    await ph.call('config/batchWrite', {'edits':[
        {'keyPath':'model_reasoning_effort','value':None,'mergeStrategy':'replace'}]})
    assert (await ph.call('config/read'))['config']['model_reasoning_effort'] is None

async def test_terminal_error_keeps_client_and_accepts_fresh_user_turn(ph, monkeypatch):
    cls=ph.sdk.ClaudeSDKClient
    original=cls.query
    async def query(client,prompt):
        text,prompt=await materialize(prompt)
        if text=='FAIL_RESULT':
            client.query_count+=1
            await client.events.put({'type':'result','session_id':client.options.resume or client.options.session_id,
                'subtype':'error_max_turns','is_error':True,'num_turns':13,'usage':{}})
        else:
            await original(client,prompt)
    monkeypatch.setattr(cls,'query',query)
    tid=await ph.thread()
    old=await ph.turn(tid,'FAIL_RESULT');await ph.drain()
    assert ph.store.turn(old)['status']=='failed'
    before=ph.engine.runtime_status(tid)
    assert before['ownerTaskAlive'] and before['state']=='waiting'
    following=await ph.turn(tid,'fresh user input');await ph.drain()
    assert ph.store.turn(following)['status']=='completed'
    assert ph.engine.runtime_status(tid)['processId']==before['processId']
    assert len(ph.sdk.clients)==1 and ph.sdk.clients[0].query_count==2


async def test_status_survives_closed_sdk_process_handle(ph):
    tid = await ph.thread()
    await ph.call('claude/runtime/start', {'threadId': tid})
    owner = ph.provider.owners[tid]
    original = owner.process
    class ClosedHandle:
        @property
        def pid(self): raise ValueError('closed handle')
        @property
        def returncode(self): raise ValueError('closed handle')
    try:
        owner.process = ClosedHandle()
        status = await ph.call('claude/runtime/status', {'threadId': tid})
        assert status['processId'] is None and status['processExitCode'] is None
        assert status['processExitConfirmed'] is not True
    finally:
        owner.process = original

async def test_internal_rpc_error_records_stack_without_payload(ph):
    original = ph.server.dispatch
    async def broken(*args): raise ValueError('SECRET EXCEPTION BODY')
    ph.server.dispatch = broken
    try:
        result = await ph.call('claude/runtime/status', {'threadId':'fixture','secret':'SECRET REQUEST'}, error=True)
        assert result['code'] == -32603 and 'diagnostic' in result['message']
        content = (ph.settings.state_dir/'rpc-errors.jsonl').read_text()
        assert 'ValueError' in content and 'broken' in content
        assert 'SECRET' not in content and 'SECRET' not in result['message']
    finally:
        ph.server.dispatch = original


async def test_assistant_error_drains_result_without_destroying_runtime(ph, monkeypatch):
    cls=ph.sdk.ClaudeSDKClient
    original=cls.query
    async def query(client,prompt):
        text,prompt=await materialize(prompt)
        if text=='FAIL_RESULT':
            client.query_count+=1
            await client.events.put({'type':'assistant','error':'server_error','content':[]})
            await client.events.put({'type':'result','session_id':client.options.resume or client.options.session_id,
                'subtype':'error_max_turns','is_error':True,'num_turns':13,'usage':{}})
        else:
            await original(client,prompt)
    monkeypatch.setattr(cls,'query',query)
    tid=await ph.thread()
    old=await ph.turn(tid,'FAIL_RESULT');await ph.drain()
    assert ph.store.turn(old)['status']=='failed'
    assert 'server_error' in ph.store.turn(old)['error']['message']
    before=ph.engine.runtime_status(tid)
    assert before['ownerTaskAlive'] and before['state']=='waiting'
    following=await ph.turn(tid,'fresh user input');await ph.drain()
    assert ph.store.turn(following)['status']=='completed'
    assert ph.engine.runtime_status(tid)['processId']==before['processId']
    assert len(ph.sdk.clients)==1 and ph.sdk.clients[0].query_count==2
